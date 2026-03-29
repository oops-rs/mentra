use serde_json::json;

use crate::{
    ContentBlock,
    agent::{Agent, AgentEvent, CompactionTrigger, SpawnedAgentStatus},
    memory::{MemorySearchMode, MemorySearchRequest},
    tool::{
        ParallelToolContext, ToolCall, ToolContext, ToolResult,
        internal::content_block_to_tool_result,
    },
    transcript::{DelegationArtifact, DelegationEdge, DelegationKind, DelegationStatus},
};

use super::{RuntimeIntrinsicTool, descriptor::runtime_intrinsic_descriptor};

pub(super) async fn execute_parallel(
    tool: RuntimeIntrinsicTool,
    ctx: ParallelToolContext,
    input: serde_json::Value,
) -> ToolResult {
    match tool {
        RuntimeIntrinsicTool::MemorySearch => execute_memory_search(ctx, input).await,
        _ => Err(format!(
            "Tool '{}' does not support parallel execution",
            runtime_intrinsic_descriptor(tool).provider.name
        )),
    }
}

pub(super) async fn execute_mut(
    tool: RuntimeIntrinsicTool,
    ctx: ToolContext<'_>,
    input: serde_json::Value,
) -> ToolResult {
    match tool {
        RuntimeIntrinsicTool::MemorySearch => execute_memory_search(ctx.into(), input).await,
        _ => {
            let call = ToolCall {
                id: ctx.tool_call_id.clone(),
                name: runtime_intrinsic_descriptor(tool).provider.name,
                input,
            };
            let block = match tool {
                RuntimeIntrinsicTool::Compact => execute_compact(ctx.agent, call).await,
                RuntimeIntrinsicTool::Idle => execute_idle(ctx.agent, call),
                RuntimeIntrinsicTool::MemorySearch => unreachable!("handled above"),
                RuntimeIntrinsicTool::MemoryPin => execute_memory_pin(ctx, call),
                RuntimeIntrinsicTool::MemoryForget => execute_memory_forget(ctx, call),
                RuntimeIntrinsicTool::Task => execute_task(ctx.agent, call).await,
            };
            content_block_to_tool_result("Runtime intrinsic", block)
        }
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
