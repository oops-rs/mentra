use async_trait::async_trait;
use serde_json::json;

use crate::{
    ContentBlock,
    runtime::{Agent, AgentEvent, ContextCompactionTrigger, SpawnedAgentStatus, task, team},
    tool::{
        ExecutableTool, ToolCall, ToolCapability, ToolContext, ToolDurability, ToolResult,
        ToolSideEffectLevel, ToolSpec,
    },
};

pub(crate) const COMPACT_TOOL_NAME: &str = "compact";
pub(crate) const IDLE_TOOL_NAME: &str = "idle";
pub(crate) const TASK_TOOL_NAME: &str = "task";

fn intrinsic_spec(
    name: &str,
    description: &str,
    input_schema: serde_json::Value,
    capabilities: Vec<ToolCapability>,
    side_effect_level: ToolSideEffectLevel,
    durability: ToolDurability,
) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: Some(description.to_string()),
        input_schema,
        capabilities,
        side_effect_level,
        durability,
    }
}

pub(crate) fn specs() -> Vec<ToolSpec> {
    vec![
        intrinsic_spec(
            COMPACT_TOOL_NAME,
            "Compress older conversation context into a summary.",
            json!({
                "type": "object",
                "properties": {}
            }),
            vec![ToolCapability::ContextCompaction],
            ToolSideEffectLevel::LocalState,
            ToolDurability::Persistent,
        ),
        intrinsic_spec(
            IDLE_TOOL_NAME,
            "Yield the current turn and return to the teammate idle loop.",
            json!({
                "type": "object",
                "properties": {}
            }),
            vec![ToolCapability::Delegation],
            ToolSideEffectLevel::LocalState,
            ToolDurability::Persistent,
        ),
        intrinsic_spec(
            TASK_TOOL_NAME,
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
        ),
    ]
    .into_iter()
    .chain(task::intrinsic_specs())
    .chain(team::intrinsic_specs())
    .collect()
}

#[derive(Clone, Copy)]
pub(crate) enum RuntimeIntrinsicTool {
    Compact,
    Idle,
    Task,
}

impl RuntimeIntrinsicTool {
    fn all() -> [Self; 3] {
        [Self::Compact, Self::Idle, Self::Task]
    }

    fn spec(self) -> ToolSpec {
        match self {
            Self::Compact => specs()[0].clone(),
            Self::Idle => specs()[1].clone(),
            Self::Task => specs()[2].clone(),
        }
    }
}

#[async_trait]
impl ExecutableTool for RuntimeIntrinsicTool {
    fn spec(&self) -> ToolSpec {
        (*self).spec()
    }

    async fn execute(&self, ctx: ToolContext<'_>, input: serde_json::Value) -> ToolResult {
        let call = ToolCall {
            id: ctx.tool_call_id.clone(),
            name: self.spec().name,
            input,
        };
        let block = match self {
            Self::Compact => execute_compact(ctx.agent, call).await,
            Self::Idle => execute_idle(ctx.agent, call),
            Self::Task => execute_task(ctx.agent, call).await,
        };
        content_block_to_result(block)
    }
}

pub(crate) fn register_tools(registry: &mut crate::tool::ToolRegistry) {
    for tool in RuntimeIntrinsicTool::all() {
        registry.register_tool(tool);
    }
    task::register_tools(registry);
    team::register_tools(registry);
}

fn content_block_to_result(block: ContentBlock) -> ToolResult {
    match block {
        ContentBlock::ToolResult {
            content, is_error, ..
        } => {
            if is_error {
                Err(content)
            } else {
                Ok(content)
            }
        }
        _ => Err("Runtime intrinsic returned an unexpected content block".to_string()),
    }
}

fn execute_idle(agent: &mut Agent, call: ToolCall) -> ContentBlock {
    agent.request_idle();
    ContentBlock::ToolResult {
        tool_use_id: call.id,
        content: "Yielding to the teammate idle loop.".to_string(),
        is_error: false,
    }
}

async fn execute_compact(agent: &mut Agent, call: ToolCall) -> ContentBlock {
    match agent
        .compact_history(
            agent.history().len().saturating_sub(1),
            ContextCompactionTrigger::Manual,
        )
        .await
    {
        Ok(Some(details)) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!(
                "Context compacted. Transcript saved to {}",
                details.transcript_path.display()
            ),
            is_error: false,
        },
        Ok(None) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: "Context compaction skipped because there was no older history to summarize."
                .to_string(),
            is_error: false,
        },
        Err(error) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Context compaction failed: {error:?}"),
            is_error: true,
        },
    }
}

async fn execute_task(agent: &mut Agent, call: ToolCall) -> ContentBlock {
    match crate::runtime::agent::parse_task_input(call.input) {
        Ok(prompt) => {
            let mut child = match agent.spawn_subagent() {
                Ok(child) => child,
                Err(error) => {
                    return ContentBlock::ToolResult {
                        tool_use_id: call.id,
                        content: format!("Failed to spawn subagent: {error:?}"),
                        is_error: true,
                    };
                }
            };
            let started = agent.register_subagent(&child);
            agent.emit_event(AgentEvent::SubagentSpawned { agent: started });

            match Box::pin(child.send(vec![ContentBlock::Text { text: prompt }])).await {
                Ok(()) => {
                    if let Some(finished) =
                        agent.finish_subagent(child.id(), SpawnedAgentStatus::Finished)
                    {
                        agent.emit_event(AgentEvent::SubagentFinished { agent: finished });
                    }
                    if let Err(error) = agent.refresh_tasks_from_disk() {
                        return ContentBlock::ToolResult {
                            tool_use_id: call.id,
                            content: format!("Task refresh failed: {error:?}"),
                            is_error: true,
                        };
                    }

                    ContentBlock::ToolResult {
                        tool_use_id: call.id,
                        content: child.final_text_summary(),
                        is_error: false,
                    }
                }
                Err(error) => {
                    if let Some(finished) = agent.finish_subagent(
                        child.id(),
                        SpawnedAgentStatus::Failed(format!("{error:?}")),
                    ) {
                        agent.emit_event(AgentEvent::SubagentFinished { agent: finished });
                    }
                    let _ = agent.refresh_tasks_from_disk();

                    ContentBlock::ToolResult {
                        tool_use_id: call.id,
                        content: format!("Subagent failed: {error:?}"),
                        is_error: true,
                    }
                }
            }
        }
        Err(content) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content,
            is_error: true,
        },
    }
}
