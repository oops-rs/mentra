use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{
    ContentBlock,
    runtime::{
        Agent, TeamProtocolStatus,
        error::RuntimeError,
        team::{
            TEAM_BROADCAST_TOOL_NAME, TEAM_LIST_REQUESTS_TOOL_NAME, TEAM_READ_INBOX_TOOL_NAME,
            TEAM_REQUEST_TOOL_NAME, TEAM_RESPOND_TOOL_NAME, TEAM_SEND_TOOL_NAME,
            TEAM_SPAWN_TOOL_NAME, TeamRequestDirection,
        },
    },
    tool::{
        ExecutableTool, ToolCall, ToolCapability, ToolContext, ToolDurability, ToolResult,
        ToolSideEffectLevel, ToolSpec,
    },
};

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

fn team_spec(name: &str, description: &str, input_schema: serde_json::Value) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: Some(description.to_string()),
        input_schema,
        capabilities: vec![ToolCapability::TeamCoordination],
        side_effect_level: ToolSideEffectLevel::LocalState,
        durability: ToolDurability::Persistent,
    }
}

pub(crate) fn intrinsic_specs() -> Vec<ToolSpec> {
    vec![
        team_spec(
            TEAM_SPAWN_TOOL_NAME,
            "Create a persistent teammate that can receive mailbox messages across turns.",
            json!({
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
        ),
        team_spec(
            TEAM_SEND_TOOL_NAME,
            "Send a normal mailbox message to the lead or a persistent teammate. Use this to ask a teammate for work or a proposal; do not use team_request when you are simply asking them to submit a plan back to you.",
            json!({
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
        ),
        team_spec(
            TEAM_READ_INBOX_TOOL_NAME,
            "Read and drain any currently pending mailbox messages for this agent.",
            json!({
                "type": "object",
                "properties": {}
            }),
        ),
        team_spec(
            TEAM_BROADCAST_TOOL_NAME,
            "Lead-only team announcement tool. Send the same mailbox message to every other known agent on the team.",
            json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "Message body to deliver to every other teammate"
                    }
                },
                "required": ["content"]
            }),
        ),
        team_spec(
            TEAM_REQUEST_TOOL_NAME,
            "Create a structured team request with a generated request_id and durable status. Use this when you are the requester and expect the other side to answer with team_respond. For built-in plan review, the teammate doing risky work should send protocol `plan_approval` to the lead; the lead should usually ask for the plan with team_send, then answer the inbound request with team_respond.",
            json!({
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
        ),
        team_spec(
            TEAM_RESPOND_TOOL_NAME,
            "Approve or reject a pending team request by request_id.",
            json!({
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
        ),
        team_spec(
            TEAM_LIST_REQUESTS_TOOL_NAME,
            "List visible team protocol requests with optional filters.",
            json!({
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
        ),
    ]
}

#[derive(Clone, Copy)]
pub(crate) enum TeamIntrinsicTool {
    Spawn,
    Send,
    ReadInbox,
    Broadcast,
    Request,
    Respond,
    ListRequests,
}

impl TeamIntrinsicTool {
    fn all() -> [Self; 7] {
        [
            Self::Spawn,
            Self::Send,
            Self::ReadInbox,
            Self::Broadcast,
            Self::Request,
            Self::Respond,
            Self::ListRequests,
        ]
    }

    fn spec(self) -> ToolSpec {
        match self {
            Self::Spawn => intrinsic_specs()[0].clone(),
            Self::Send => intrinsic_specs()[1].clone(),
            Self::ReadInbox => intrinsic_specs()[2].clone(),
            Self::Broadcast => intrinsic_specs()[3].clone(),
            Self::Request => intrinsic_specs()[4].clone(),
            Self::Respond => intrinsic_specs()[5].clone(),
            Self::ListRequests => intrinsic_specs()[6].clone(),
        }
    }
}

#[async_trait]
impl ExecutableTool for TeamIntrinsicTool {
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
            Self::Spawn => execute_team_spawn(ctx.agent, call).await,
            Self::Send => execute_team_send(ctx.agent, call),
            Self::ReadInbox => execute_team_read_inbox(ctx.agent, call),
            Self::Broadcast => execute_team_broadcast(ctx.agent, call),
            Self::Request => execute_team_request(ctx.agent, call),
            Self::Respond => execute_team_respond(ctx.agent, call),
            Self::ListRequests => execute_team_list_requests(ctx.agent, call),
        };
        content_block_to_result(block)
    }
}

pub(crate) fn register_tools(registry: &mut crate::tool::ToolRegistry) {
    for tool in TeamIntrinsicTool::all() {
        registry.register_tool(tool);
    }
}

pub(crate) async fn execute_team_spawn(agent: &mut Agent, call: ToolCall) -> ContentBlock {
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

pub(crate) fn execute_team_send(agent: &mut Agent, call: ToolCall) -> ContentBlock {
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

pub(crate) fn execute_team_read_inbox(agent: &mut Agent, call: ToolCall) -> ContentBlock {
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

pub(crate) fn execute_team_broadcast(agent: &mut Agent, call: ToolCall) -> ContentBlock {
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

pub(crate) fn execute_team_request(agent: &mut Agent, call: ToolCall) -> ContentBlock {
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

pub(crate) fn execute_team_respond(agent: &mut Agent, call: ToolCall) -> ContentBlock {
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

pub(crate) fn execute_team_list_requests(agent: &mut Agent, call: ToolCall) -> ContentBlock {
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
        _ => Err("Team intrinsic returned an unexpected content block".to_string()),
    }
}
