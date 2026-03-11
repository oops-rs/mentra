use serde::Deserialize;
use serde_json::json;

use crate::{
    ContentBlock,
    runtime::{
        Agent, AgentEvent, ContextCompactionTrigger, SpawnedAgentStatus, TASK_CREATE_TOOL_NAME,
        TASK_GET_TOOL_NAME, TASK_LIST_TOOL_NAME, TASK_UPDATE_TOOL_NAME, TEAM_BROADCAST_TOOL_NAME,
        TEAM_LIST_REQUESTS_TOOL_NAME, TEAM_READ_INBOX_TOOL_NAME, TEAM_REQUEST_TOOL_NAME,
        TEAM_RESPOND_TOOL_NAME, TEAM_SEND_TOOL_NAME, TEAM_SPAWN_TOOL_NAME, TeamProtocolStatus,
        error::RuntimeError, task, task_graph, team::TeamRequestDirection,
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
                "Send a normal mailbox message to the lead or a persistent teammate. Use this to ask a teammate for work or a proposal; do not use team_request when you are simply asking them to submit a plan back to you."
                    .into(),
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
            name: TEAM_BROADCAST_TOOL_NAME.to_string(),
            description: Some(
                "Lead-only team announcement tool. Send the same mailbox message to every other known agent on the team.".into(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "Message body to deliver to every other teammate"
                    }
                },
                "required": ["content"]
            }),
        },
        ToolSpec {
            name: TEAM_REQUEST_TOOL_NAME.to_string(),
            description: Some(
                "Create a structured team request with a generated request_id and durable status. Use this when you are the requester and expect the other side to answer with team_respond. For built-in plan review, the teammate doing risky work should send protocol `plan_approval` to the lead; the lead should usually ask for the plan with team_send, then answer the inbound request with team_respond."
                    .into(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "to": {
                        "type": "string",
                        "description": "Recipient teammate or lead name"
                    },
                    "protocol": {
                        "type": "string",
                        "description": "Open-ended protocol kind such as shutdown or plan_approval"
                    },
                    "content": {
                        "type": "string",
                        "description": "Request body or plan text"
                    }
                },
                "required": ["to", "protocol", "content"]
            }),
        },
        ToolSpec {
            name: TEAM_RESPOND_TOOL_NAME.to_string(),
            description: Some(
                "Approve or reject a pending team request by request_id.".into(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "request_id": {
                        "type": "string",
                        "description": "Correlated request identifier"
                    },
                    "approve": {
                        "type": "boolean",
                        "description": "Whether to approve the request"
                    },
                    "reason": {
                        "type": "string",
                        "description": "Optional explanation or feedback"
                    }
                },
                "required": ["request_id", "approve"]
            }),
        },
        ToolSpec {
            name: TEAM_LIST_REQUESTS_TOOL_NAME.to_string(),
            description: Some(
                "List visible team protocol requests with optional filters.".into(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "enum": ["pending", "approved", "rejected"],
                        "description": "Optional request status filter"
                    },
                    "protocol": {
                        "type": "string",
                        "description": "Optional protocol kind filter"
                    },
                    "counterparty": {
                        "type": "string",
                        "description": "Optional other participant filter"
                    },
                    "direction": {
                        "type": "string",
                        "enum": ["inbound", "outbound", "any"],
                        "description": "Filter relative to the current agent"
                    }
                }
            }),
        },
        ToolSpec {
            name: TASK_CREATE_TOOL_NAME.to_string(),
            description: Some(
                "Lead-oriented project planning tool. Create a persisted task in the task graph."
                    .into(),
            ),
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
            description: Some(
                "Lead-oriented project planning tool. Update a persisted task and its dependency edges."
                    .into(),
            ),
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
        TEAM_BROADCAST_TOOL_NAME => Some(IntrinsicOutcome {
            result: execute_team_broadcast(agent, call),
            touched_task_graph: false,
        }),
        TEAM_REQUEST_TOOL_NAME => Some(IntrinsicOutcome {
            result: execute_team_request(agent, call),
            touched_task_graph: false,
        }),
        TEAM_RESPOND_TOOL_NAME => Some(IntrinsicOutcome {
            result: execute_team_respond(agent, call),
            touched_task_graph: false,
        }),
        TEAM_LIST_REQUESTS_TOOL_NAME => Some(IntrinsicOutcome {
            result: execute_team_list_requests(agent, call),
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

#[derive(Debug, Deserialize)]
struct TeamBroadcastInput {
    content: String,
}

#[derive(Debug, Deserialize)]
struct TeamRequestInput {
    to: String,
    protocol: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct TeamRespondInput {
    request_id: String,
    approve: bool,
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TeamListRequestsInput {
    status: Option<String>,
    protocol: Option<String>,
    counterparty: Option<String>,
    direction: Option<String>,
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

    match agent
        .spawn_teammate(input.name, input.role, input.prompt)
        .await
    {
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

fn execute_team_broadcast(agent: &mut Agent, call: ToolCall) -> ContentBlock {
    let input = match serde_json::from_value::<TeamBroadcastInput>(call.input) {
        Ok(input) => input,
        Err(error) => {
            return ContentBlock::ToolResult {
                tool_use_id: call.id,
                content: format!("Invalid broadcast input: {error}"),
                is_error: true,
            };
        }
    };

    match agent.broadcast_team_message(input.content) {
        Ok(dispatches) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!(
                "Broadcast message sent to {} recipient(s): {}",
                dispatches.len(),
                dispatches
                    .into_iter()
                    .map(|dispatch| dispatch.teammate)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            is_error: false,
        },
        Err(error) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Failed to broadcast team message: {error:?}"),
            is_error: true,
        },
    }
}

fn execute_team_request(agent: &mut Agent, call: ToolCall) -> ContentBlock {
    let input = match serde_json::from_value::<TeamRequestInput>(call.input) {
        Ok(input) => input,
        Err(error) => {
            return ContentBlock::ToolResult {
                tool_use_id: call.id,
                content: format!("Invalid team_request input: {error}"),
                is_error: true,
            };
        }
    };

    match agent.request_team_protocol(&input.to, input.protocol, input.content) {
        Ok(request) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!(
                "Created team request '{}' for '{}' using protocol '{}'",
                request.request_id, request.to, request.protocol
            ),
            is_error: false,
        },
        Err(error) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Failed to create team request: {error:?}"),
            is_error: true,
        },
    }
}

fn execute_team_respond(agent: &mut Agent, call: ToolCall) -> ContentBlock {
    let input = match serde_json::from_value::<TeamRespondInput>(call.input) {
        Ok(input) => input,
        Err(error) => {
            return ContentBlock::ToolResult {
                tool_use_id: call.id,
                content: format!("Invalid team_respond input: {error}"),
                is_error: true,
            };
        }
    };

    match agent.respond_team_protocol(&input.request_id, input.approve, input.reason) {
        Ok(request) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!(
                "{} team request '{}' ({})",
                if input.approve {
                    "Approved"
                } else {
                    "Rejected"
                },
                request.request_id,
                request.protocol
            ),
            is_error: false,
        },
        Err(error) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Failed to respond to team request: {error:?}"),
            is_error: true,
        },
    }
}

fn execute_team_list_requests(agent: &mut Agent, call: ToolCall) -> ContentBlock {
    let input = match serde_json::from_value::<TeamListRequestsInput>(call.input) {
        Ok(input) => input,
        Err(error) => {
            return ContentBlock::ToolResult {
                tool_use_id: call.id,
                content: format!("Invalid team_list_requests input: {error}"),
                is_error: true,
            };
        }
    };

    let status = match input.status.as_deref() {
        Some("pending") => Some(TeamProtocolStatus::Pending),
        Some("approved") => Some(TeamProtocolStatus::Approved),
        Some("rejected") => Some(TeamProtocolStatus::Rejected),
        Some(value) => {
            return ContentBlock::ToolResult {
                tool_use_id: call.id,
                content: format!("Invalid team_list_requests status '{value}'"),
                is_error: true,
            };
        }
        None => None,
    };

    let direction = match input.direction.as_deref() {
        Some("inbound") => TeamRequestDirection::Inbound,
        Some("outbound") => TeamRequestDirection::Outbound,
        Some("any") | None => TeamRequestDirection::Any,
        Some(value) => {
            return ContentBlock::ToolResult {
                tool_use_id: call.id,
                content: format!("Invalid team_list_requests direction '{value}'"),
                is_error: true,
            };
        }
    };

    match agent.list_team_protocol_requests(status, input.protocol, input.counterparty, direction) {
        Ok(requests) => match serde_json::to_string_pretty(&requests) {
            Ok(content) => ContentBlock::ToolResult {
                tool_use_id: call.id,
                content,
                is_error: false,
            },
            Err(error) => ContentBlock::ToolResult {
                tool_use_id: call.id,
                content: format!("Failed to serialize team requests: {error:?}"),
                is_error: true,
            },
        },
        Err(error) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Failed to list team requests: {error:?}"),
            is_error: true,
        },
    }
}
