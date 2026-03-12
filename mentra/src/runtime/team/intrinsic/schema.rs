use serde::Deserialize;
use serde_json::json;

use crate::{
    runtime::team::{
        TEAM_BROADCAST_TOOL_NAME, TEAM_LIST_REQUESTS_TOOL_NAME, TEAM_READ_INBOX_TOOL_NAME,
        TEAM_REQUEST_TOOL_NAME, TEAM_RESPOND_TOOL_NAME, TEAM_SEND_TOOL_NAME, TEAM_SPAWN_TOOL_NAME,
    },
    tool::{ToolCapability, ToolDurability, ToolSideEffectLevel, ToolSpec},
};

use super::TeamIntrinsicTool;

#[derive(Debug, Deserialize)]
pub(super) struct TeamSpawnInput {
    pub(super) name: String,
    pub(super) role: String,
    pub(super) prompt: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct TeamSendInput {
    pub(super) to: String,
    pub(super) content: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct TeamBroadcastInput {
    pub(super) content: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct TeamRequestInput {
    pub(super) to: String,
    pub(super) protocol: String,
    pub(super) content: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct TeamRespondInput {
    pub(super) request_id: String,
    pub(super) approve: bool,
    pub(super) reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct TeamListRequestsInput {
    pub(super) status: Option<String>,
    pub(super) protocol: Option<String>,
    pub(super) counterparty: Option<String>,
    pub(super) direction: Option<String>,
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

pub(super) fn tool_spec(tool: TeamIntrinsicTool) -> ToolSpec {
    match tool {
        TeamIntrinsicTool::Spawn => team_spec(
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
        TeamIntrinsicTool::Send => team_spec(
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
        TeamIntrinsicTool::ReadInbox => team_spec(
            TEAM_READ_INBOX_TOOL_NAME,
            "Read and drain any currently pending mailbox messages for this agent.",
            json!({
                "type": "object",
                "properties": {}
            }),
        ),
        TeamIntrinsicTool::Broadcast => team_spec(
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
        TeamIntrinsicTool::Request => team_spec(
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
        TeamIntrinsicTool::Respond => team_spec(
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
        TeamIntrinsicTool::ListRequests => team_spec(
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
    }
}

pub(crate) fn intrinsic_specs() -> Vec<ToolSpec> {
    TeamIntrinsicTool::all()
        .into_iter()
        .map(tool_spec)
        .collect()
}
