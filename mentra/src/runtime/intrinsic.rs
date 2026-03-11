use serde_json::json;
use serde::Deserialize;

use crate::{
    ContentBlock,
    runtime::{
        Agent, AgentEvent, ContextCompactionTrigger, SpawnedAgentStatus, TASK_CREATE_TOOL_NAME,
        TASK_GET_TOOL_NAME, TASK_LIST_TOOL_NAME, TASK_UPDATE_TOOL_NAME,
        TEAM_READ_INBOX_TOOL_NAME, TEAM_SEND_TOOL_NAME, TEAM_SPAWN_TOOL_NAME, task, task_graph,
        error::RuntimeError,
    },
    tool::{ToolCall, ToolSpec},
};

pub(crate) const COMPACT_TOOL_NAME: &str = "compact";
pub(crate) const TASK_TOOL_NAME: &str = "task";

pub(crate) struct IntrinsicOutcome {
    pub(crate) result: ContentBlock,
    pub(crate) touched_task_graph: bool,
}

pub(crate) fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: COMPACT_TOOL_NAME.to_string(),
            description: Some("Compress older conversation context into a summary.".into()),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolSpec {
            name: TASK_TOOL_NAME.to_string(),
            description: Some(
                "Spawn a fresh subagent to work a subtask and return a concise summary.".into(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "Delegated task prompt for the subagent"
                    }
                },
                "required": ["prompt"]
            }),
        },
        ToolSpec {
            name: TEAM_SPAWN_TOOL_NAME.to_string(),
            description: Some(
                "Create a persistent teammate that can receive mailbox messages across turns."
                    .into(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Unique teammate name"
                    },
                    "role": {
                        "type": "string",
                        "description": "Short responsibility or specialty for this teammate"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Optional kickoff message to send immediately after spawning"
                    }
                },
                "required": ["name", "role"]
            }),
        },
        ToolSpec {
            name: TEAM_SEND_TOOL_NAME.to_string(),
            description: Some(
                "Send a mailbox message to the lead or a persistent teammate.".into(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "to": {
                        "type": "string",
                        "description": "Recipient teammate or lead name"
                    },
                    "content": {
                        "type": "string",
                        "description": "Message body to deliver"
                    }
                },
                "required": ["to", "content"]
            }),
        },
        ToolSpec {
            name: TEAM_READ_INBOX_TOOL_NAME.to_string(),
            description: Some(
                "Read and drain any currently pending mailbox messages for this agent.".into(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolSpec {
            name: TASK_CREATE_TOOL_NAME.to_string(),
            description: Some("Create a persisted task in the task graph.".into()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "subject": {
                        "type": "string",
                        "description": "Short title for the task"
                    },
                    "description": {
                        "type": "string",
                        "description": "Optional extra detail for the task"
                    },
                    "owner": {
                        "type": "string",
                        "description": "Optional owner label for the task"
                    },
                    "blockedBy": {
                        "type": "array",
                        "items": { "type": "integer" },
                        "description": "Task IDs that must finish before this task is ready"
                    }
                },
                "required": ["subject"]
            }),
        },
        ToolSpec {
            name: TASK_UPDATE_TOOL_NAME.to_string(),
            description: Some("Update a persisted task and its dependency edges.".into()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "taskId": {
                        "type": "integer",
                        "description": "Stable identifier for the task"
                    },
                    "subject": {
                        "type": "string",
                        "description": "Updated task subject"
                    },
                    "description": {
                        "type": "string",
                        "description": "Updated task description"
                    },
                    "owner": {
                        "type": "string",
                        "description": "Updated task owner"
                    },
                    "status": {
                        "type": "string",
                        "enum": ["pending", "in_progress", "completed"],
                        "description": "Updated task status"
                    },
                    "addBlockedBy": {
                        "type": "array",
                        "items": { "type": "integer" },
                        "description": "Add dependency edges from blocker tasks into this task"
                    },
                    "removeBlockedBy": {
                        "type": "array",
                        "items": { "type": "integer" },
                        "description": "Remove dependency edges from blocker tasks into this task"
                    },
                    "addBlocks": {
                        "type": "array",
                        "items": { "type": "integer" },
                        "description": "Add dependency edges from this task into dependent tasks"
                    },
                    "removeBlocks": {
                        "type": "array",
                        "items": { "type": "integer" },
                        "description": "Remove dependency edges from this task into dependent tasks"
                    }
                },
                "required": ["taskId"]
            }),
        },
        ToolSpec {
            name: TASK_LIST_TOOL_NAME.to_string(),
            description: Some("List the persisted task graph grouped by readiness.".into()),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolSpec {
            name: TASK_GET_TOOL_NAME.to_string(),
            description: Some("Get one persisted task by ID.".into()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "taskId": {
                        "type": "integer",
                        "description": "Stable identifier for the task"
                    }
                },
                "required": ["taskId"]
            }),
        },
    ]
}

pub(crate) async fn execute(agent: &mut Agent, call: ToolCall) -> Option<IntrinsicOutcome> {
    match call.name.as_str() {
        COMPACT_TOOL_NAME => Some(IntrinsicOutcome {
            result: execute_compact(agent, call).await,
            touched_task_graph: false,
        }),
        TASK_TOOL_NAME => Some(IntrinsicOutcome {
            result: execute_task(agent, call).await,
            touched_task_graph: false,
        }),
        TEAM_SPAWN_TOOL_NAME => Some(IntrinsicOutcome {
            result: execute_team_spawn(agent, call).await,
            touched_task_graph: false,
        }),
        TEAM_SEND_TOOL_NAME => Some(IntrinsicOutcome {
            result: execute_team_send(agent, call),
            touched_task_graph: false,
        }),
        TEAM_READ_INBOX_TOOL_NAME => Some(IntrinsicOutcome {
            result: execute_team_read_inbox(agent, call),
            touched_task_graph: false,
        }),
        name if task_graph::is_task_graph_tool(name) => Some(execute_task_graph(agent, call)),
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
struct TeamSpawnInput {
    name: String,
    role: String,
    prompt: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TeamSendInput {
    to: String,
    content: String,
}

fn execute_task_graph(agent: &mut Agent, call: ToolCall) -> IntrinsicOutcome {
    let output = task_graph::execute(
        &call.name,
        call.input,
        agent.config().task_graph.tasks_dir.as_path(),
    );

    match output {
        Ok(content) => match agent.refresh_tasks_from_disk() {
            Ok(()) => IntrinsicOutcome {
                result: ContentBlock::ToolResult {
                    tool_use_id: call.id,
                    content,
                    is_error: false,
                },
                touched_task_graph: true,
            },
            Err(error) => IntrinsicOutcome {
                result: ContentBlock::ToolResult {
                    tool_use_id: call.id,
                    content: format!("Task graph refresh failed: {error:?}"),
                    is_error: true,
                },
                touched_task_graph: false,
            },
        },
        Err(content) => IntrinsicOutcome {
            result: ContentBlock::ToolResult {
                tool_use_id: call.id,
                content,
                is_error: true,
            },
            touched_task_graph: false,
        },
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
    match task::parse_task_input(call.input) {
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
                            content: format!("Task graph refresh failed: {error:?}"),
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
                    if let Some(finished) = agent
                        .finish_subagent(child.id(), SpawnedAgentStatus::Failed(format!("{error:?}")))
                    {
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

async fn execute_team_spawn(agent: &mut Agent, call: ToolCall) -> ContentBlock {
    let input = match serde_json::from_value::<TeamSpawnInput>(call.input) {
        Ok(input) => input,
        Err(error) => {
            return ContentBlock::ToolResult {
                tool_use_id: call.id,
                content: format!("Invalid team_spawn input: {error}"),
                is_error: true,
            };
        }
    };

    match agent.spawn_teammate(input.name, input.role, input.prompt).await {
        Ok(teammate) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!(
                "Spawned persistent teammate '{}' (role: {}, status: {:?})",
                teammate.name, teammate.role, teammate.status
            ),
            is_error: false,
        },
        Err(error) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Failed to spawn teammate: {error:?}"),
            is_error: true,
        },
    }
}

fn execute_team_send(agent: &mut Agent, call: ToolCall) -> ContentBlock {
    let input = match serde_json::from_value::<TeamSendInput>(call.input) {
        Ok(input) => input,
        Err(error) => {
            return ContentBlock::ToolResult {
                tool_use_id: call.id,
                content: format!("Invalid team_send input: {error}"),
                is_error: true,
            };
        }
    };

    match agent.send_team_message(&input.to, input.content) {
        Ok(dispatch) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Sent message to '{}'", dispatch.teammate),
            is_error: false,
        },
        Err(error) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Failed to send team message: {error:?}"),
            is_error: true,
        },
    }
}

fn execute_team_read_inbox(agent: &mut Agent, call: ToolCall) -> ContentBlock {
    match agent.read_team_inbox().and_then(|messages| {
        serde_json::to_string_pretty(&messages).map_err(RuntimeError::FailedToSerializeTeam)
    }) {
        Ok(content) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content,
            is_error: false,
        },
        Err(error) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Failed to read team inbox: {error:?}"),
            is_error: true,
        },
    }
}
