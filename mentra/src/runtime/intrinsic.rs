use async_trait::async_trait;
use serde_json::json;
use strum::{Display, VariantArray};

use crate::{
    ContentBlock,
    agent::{Agent, AgentEvent, CompactionTrigger, SpawnedAgentStatus},
    memory::{MemorySearchMode, MemorySearchRequest},
    tool::{
        ParallelToolContext, RuntimeToolDescriptor, ToolApprovalCategory, ToolCall,
        ToolCapability, ToolContext, ToolDefinition, ToolDurability, ToolExecutionCategory,
        ToolExecutor, ToolResult, ToolSideEffectLevel,
    },
    transcript::{DelegationArtifact, DelegationEdge, DelegationKind, DelegationStatus},
};

pub(crate) fn register_tools(registry: &mut crate::tool::ToolRegistry) {
    RuntimeIntrinsicTool::VARIANTS
        .iter()
        .for_each(|tool| registry.register_tool(*tool));
    crate::runtime::task::TaskIntrinsicTool::VARIANTS
        .iter()
        .for_each(|tool| registry.register_tool(*tool));
    crate::team::TeamIntrinsicTool::VARIANTS
        .iter()
        .for_each(|tool| registry.register_tool(*tool));
}

#[derive(Display, Copy, Clone, VariantArray)]
#[strum(serialize_all = "snake_case")]
pub(crate) enum RuntimeIntrinsicTool {
    Compact,
    Idle,
    MemoryForget,
    MemoryPin,
    MemorySearch,
    Task,
}

impl RuntimeIntrinsicTool {
    fn intrinsic_spec(
        &self,
        description: &str,
        input_schema: serde_json::Value,
        capabilities: Vec<ToolCapability>,
        side_effect_level: ToolSideEffectLevel,
        durability: ToolDurability,
        execution_category: ToolExecutionCategory,
        approval_category: ToolApprovalCategory,
    ) -> RuntimeToolDescriptor {
        RuntimeToolDescriptor::builder(self.to_string())
            .description(description)
            .input_schema(input_schema)
            .capabilities(capabilities)
            .side_effect_level(side_effect_level)
            .durability(durability)
            .execution_category(execution_category)
            .approval_category(approval_category)
            .build()
    }
}

impl ToolDefinition for RuntimeIntrinsicTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        match self {
            Self::Compact => self.intrinsic_spec(
                "Compress older conversation context into a summary.",
                json!({
                    "type": "object",
                    "properties": {}
                }),
                vec![ToolCapability::ContextCompaction],
                ToolSideEffectLevel::LocalState,
                ToolDurability::Persistent,
                ToolExecutionCategory::ExclusivePersistentMutation,
                ToolApprovalCategory::Default,
            ),
            Self::Idle => self.intrinsic_spec(
                "Yield the current turn and return to the teammate idle loop.",
                json!({
                    "type": "object",
                    "properties": {}
                }),
                vec![ToolCapability::Delegation],
                ToolSideEffectLevel::LocalState,
                ToolDurability::Persistent,
                ToolExecutionCategory::Delegation,
                ToolApprovalCategory::Delegation,
            ),
            Self::MemorySearch => self.intrinsic_spec(
                "Search the current agent's long-term memory for additional recall.",
                json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Memory query text"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of results to return"
                        }
                    },
                    "required": ["query"]
                }),
                vec![ToolCapability::ReadOnly],
                ToolSideEffectLevel::None,
                ToolDurability::ReplaySafe,
                ToolExecutionCategory::ReadOnlyParallel,
                ToolApprovalCategory::ReadOnly,
            ),
            Self::MemoryPin => self.intrinsic_spec(
                "Persist a fact in long-term memory for the current agent.",
                json!({
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "Fact to remember"
                        }
                    },
                    "required": ["content"]
                }),
                vec![ToolCapability::Custom("memory_write".to_string())],
                ToolSideEffectLevel::LocalState,
                ToolDurability::Persistent,
                ToolExecutionCategory::ExclusivePersistentMutation,
                ToolApprovalCategory::Default,
            ),
            Self::MemoryForget => self.intrinsic_spec(
                "Forget a specific long-term memory record by id.",
                json!({
                    "type": "object",
                    "properties": {
                        "record_id": {
                            "type": "string",
                            "description": "Identifier of the memory record to forget"
                        }
                    },
                    "required": ["record_id"]
                }),
                vec![ToolCapability::Custom("memory_write".to_string())],
                ToolSideEffectLevel::LocalState,
                ToolDurability::Persistent,
                ToolExecutionCategory::ExclusivePersistentMutation,
                ToolApprovalCategory::Default,
            ),
            Self::Task => self.intrinsic_spec(
                "Spawn a fresh subagent to work a subtask and return a concise summary.",
                json!({
                    "type": "object",
                    "properties": {
                        "prompt": {
                            "type": "string",
                            "description": "Delegated task prompt for the subagent"
                        }
                    },
                    "required": ["prompt"]
                }),
                vec![ToolCapability::Delegation],
                ToolSideEffectLevel::LocalState,
                ToolDurability::Ephemeral,
                ToolExecutionCategory::Delegation,
                ToolApprovalCategory::Delegation,
            ),
        }
    }
}

#[async_trait]
impl ToolExecutor for RuntimeIntrinsicTool {
    async fn execute(&self, ctx: ParallelToolContext, input: serde_json::Value) -> ToolResult {
        match self {
            Self::MemorySearch => execute_memory_search(ctx, input).await,
            _ => Err(format!(
                "Tool '{}' does not support parallel execution",
                self.descriptor().provider.name
            )),
        }
    }

    async fn execute_mut(&self, ctx: ToolContext<'_>, input: serde_json::Value) -> ToolResult {
        match self {
            Self::MemorySearch => execute_memory_search(ctx.into(), input).await,
            _ => {
                let call = ToolCall {
                    id: ctx.tool_call_id.clone(),
                    name: self.descriptor().provider.name,
                    input,
                };
                let block = match self {
                    Self::Compact => execute_compact(ctx.agent, call).await,
                    Self::Idle => execute_idle(ctx.agent, call),
                    Self::MemorySearch => unreachable!("handled above"),
                    Self::MemoryPin => execute_memory_pin(ctx, call),
                    Self::MemoryForget => execute_memory_forget(ctx, call),
                    Self::Task => execute_task(ctx.agent, call).await,
                };
                content_block_to_result(block)
            }
        }
    }
}

fn content_block_to_result(block: ContentBlock) -> ToolResult {
    match block {
        ContentBlock::ToolResult {
            content, is_error, ..
        } => {
            if is_error {
                Err(content.to_display_string())
            } else {
                Ok(content.to_display_string())
            }
        }
        _ => Err("Runtime intrinsic returned an unexpected content block".to_string()),
    }
}

fn execute_idle(agent: &mut Agent, call: ToolCall) -> ContentBlock {
    agent.request_idle();
    ContentBlock::ToolResult {
        tool_use_id: call.id,
        content: "Yielding to the teammate idle loop.".into(),
        is_error: false,
    }
}

fn execute_memory_pin(ctx: ToolContext<'_>, call: ToolCall) -> ContentBlock {
    if !ctx.agent.config().memory.write_tools_enabled {
        return ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: "Memory write tools are disabled for this agent.".into(),
            is_error: true,
        };
    }

    let Some(content) = call
        .input
        .get("content")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: "Invalid memory_pin input: content is required.".into(),
            is_error: true,
        };
    };

    match ctx
        .agent
        .memory_engine()
        .pin(ctx.agent.id(), ctx.agent.memory_revision(), content)
    {
        Ok(record) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Pinned memory {}.", record.record_id).into(),
            is_error: false,
        },
        Err(error) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Failed to pin memory: {error}").into(),
            is_error: true,
        },
    }
}

fn execute_memory_forget(ctx: ToolContext<'_>, call: ToolCall) -> ContentBlock {
    if !ctx.agent.config().memory.write_tools_enabled {
        return ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: "Memory write tools are disabled for this agent.".into(),
            is_error: true,
        };
    }

    let Some(record_id) = call
        .input
        .get("record_id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: "Invalid memory_forget input: record_id is required.".into(),
            is_error: true,
        };
    };

    match ctx.agent.memory_engine().forget(ctx.agent.id(), record_id) {
        Ok(true) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Forgot memory {record_id}.").into(),
            is_error: false,
        },
        Ok(false) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Memory record {record_id} was not found for this agent.").into(),
            is_error: true,
        },
        Err(error) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Failed to forget memory: {error}").into(),
            is_error: true,
        },
    }
}

async fn execute_memory_search(ctx: ParallelToolContext, input: serde_json::Value) -> ToolResult {
    let Some(query) = input
        .get("query")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Err("Invalid memory_search input: query is required.".to_string());
    };

    let configured_limit = ctx
        .runtime
        .agent_config(&ctx.agent_id)?
        .memory_tool_search_limit;
    let requested_limit = input
        .get("limit")
        .and_then(|value| value.as_u64())
        .unwrap_or(configured_limit as u64) as usize;
    let limit = requested_limit.min(configured_limit).min(10);

    match ctx
        .runtime
        .memory_engine()
        .search(MemorySearchRequest {
            agent_id: ctx.agent_id.clone(),
            query: query.to_string(),
            limit,
            char_budget: None,
            mode: MemorySearchMode::Tool,
        })
        .await
    {
        Ok(hits) => {
            let results = hits
                .into_iter()
                .map(|hit| {
                    json!({
                        "id": hit.record_id,
                        "kind": hit.kind,
                        "content": hit.content,
                        "score": hit.score,
                        "timestamp": hit.created_at,
                        "source": hit.source,
                        "why_retrieved": hit.why_retrieved,
                    })
                })
                .collect::<Vec<_>>();
            Ok(serde_json::to_string_pretty(&results).unwrap_or_else(|_| "[]".to_string()))
        }
        Err(error) => Err(format!("Memory search failed: {error}")),
    }
}

async fn execute_compact(agent: &mut Agent, call: ToolCall) -> ContentBlock {
    match agent
        .compact_history(
            agent.history().len().saturating_sub(1),
            CompactionTrigger::Manual,
        )
        .await
    {
        Ok(Some(details)) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!(
                "Context compacted. Transcript saved to {}",
                details.transcript_path.display()
            )
            .into(),
            is_error: false,
        },
        Ok(None) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: "Context compaction skipped because there was no older history to summarize."
                .into(),
            is_error: false,
        },
        Err(error) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Context compaction failed: {error}").into(),
            is_error: true,
        },
    }
}

async fn execute_task(agent: &mut Agent, call: ToolCall) -> ContentBlock {
    match crate::agent::parse_task_input(call.input) {
        Ok(prompt) => {
            let task_summary = prompt.clone();
            let mut child = match agent.spawn_subagent() {
                Ok(child) => child,
                Err(error) => {
                    return ContentBlock::ToolResult {
                        tool_use_id: call.id,
                        content: format!("Failed to spawn subagent: {error}").into(),
                        is_error: true,
                    };
                }
            };
            let child_id = child.id().to_string();
            let child_name = child.name().to_string();
            let child_model = child.model().to_string();
            let edge = Some(DelegationEdge {
                kind: DelegationKind::Subagent,
                local_agent_id: agent.id().to_string(),
                remote_agent_id: child_id.clone(),
            });
            let _ = agent.record_delegation_request(
                format!(
                    "<delegation-request agent=\"{child_name}\" model=\"{child_model}\">\n{prompt}\n</delegation-request>"
                ),
                DelegationArtifact {
                    kind: DelegationKind::Subagent,
                    agent_id: child_id.clone(),
                    agent_name: child_name.clone(),
                    role: Some("subagent".to_string()),
                    status: DelegationStatus::Requested,
                    task_summary: task_summary.clone(),
                    result_summary: None,
                    artifacts: Vec::new(),
                },
                edge.clone(),
            );
            agent.sync_memory_snapshot();
            let started = agent.register_subagent(&child);
            agent.emit_event(AgentEvent::SubagentSpawned { agent: started });

            match Box::pin(child.send(vec![ContentBlock::Text { text: prompt }])).await {
                Ok(message) => {
                    let result_summary = if message.text().is_empty() {
                        child.final_text_summary()
                    } else {
                        message.text()
                    };
                    let _ = agent.record_delegation_result(
                        format!(
                            "<delegation-result agent=\"{child_name}\" status=\"finished\">\n{result_summary}\n</delegation-result>"
                        ),
                        DelegationArtifact {
                            kind: DelegationKind::Subagent,
                            agent_id: child_id.clone(),
                            agent_name: child_name.clone(),
                            role: Some("subagent".to_string()),
                            status: DelegationStatus::Finished,
                            task_summary: task_summary.clone(),
                            result_summary: Some(result_summary.clone()),
                            artifacts: Vec::new(),
                        },
                        edge.clone(),
                    );
                    agent.sync_memory_snapshot();
                    if let Some(finished) =
                        agent.finish_subagent(child.id(), SpawnedAgentStatus::Finished)
                    {
                        agent.emit_event(AgentEvent::SubagentFinished { agent: finished });
                    }
                    if let Err(error) = agent.refresh_tasks_from_disk() {
                        return ContentBlock::ToolResult {
                            tool_use_id: call.id,
                            content: format!("Task refresh failed: {error}").into(),
                            is_error: true,
                        };
                    }

                    ContentBlock::ToolResult {
                        tool_use_id: call.id,
                        content: result_summary.into(),
                        is_error: false,
                    }
                }
                Err(error) => {
                    let error_text = error.to_string();
                    let _ = agent.record_delegation_result(
                        format!(
                            "<delegation-result agent=\"{child_name}\" status=\"failed\">\n{error_text}\n</delegation-result>"
                        ),
                        DelegationArtifact {
                            kind: DelegationKind::Subagent,
                            agent_id: child_id,
                            agent_name: child_name,
                            role: Some("subagent".to_string()),
                            status: DelegationStatus::Failed,
                            task_summary,
                            result_summary: Some(error_text.clone()),
                            artifacts: Vec::new(),
                        },
                        edge,
                    );
                    agent.sync_memory_snapshot();
                    if let Some(finished) = agent
                        .finish_subagent(child.id(), SpawnedAgentStatus::Failed(error_text.clone()))
                    {
                        agent.emit_event(AgentEvent::SubagentFinished { agent: finished });
                    }
                    let _ = agent.refresh_tasks_from_disk();

                    ContentBlock::ToolResult {
                        tool_use_id: call.id,
                        content: format!("Subagent failed: {error_text}").into(),
                        is_error: true,
                    }
                }
            }
        }
        Err(content) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: content.into(),
            is_error: true,
        },
    }
}
