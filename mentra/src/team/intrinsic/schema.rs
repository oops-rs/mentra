use serde::Deserialize;
use serde_json::json;

use crate::tool::{
    RuntimeToolDescriptor, ToolApprovalCategory, ToolCapability, ToolDurability,
    ToolExecutionCategory, ToolSideEffectLevel,
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

impl TeamIntrinsicTool {
    fn team_spec(
        &self,
        description: &str,
        input_schema: serde_json::Value,
        execution_category: ToolExecutionCategory,
    ) -> RuntimeToolDescriptor {
        RuntimeToolDescriptor::builder(self.to_string())
            .description(description)
            .input_schema(input_schema)
            .capability(ToolCapability::TeamCoordination)
            .side_effect_level(ToolSideEffectLevel::LocalState)
            .durability(ToolDurability::Persistent)
            .execution_category(execution_category)
            .approval_category(ToolApprovalCategory::Delegation)
            .build()
    }

    pub(super) fn tool_spec(&self) -> RuntimeToolDescriptor {
        match self {
            TeamIntrinsicTool::Spawn => self.team_spec(
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
                ToolExecutionCategory::Delegation,
            ),
            TeamIntrinsicTool::Send => self.team_spec(
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
                ToolExecutionCategory::ExclusivePersistentMutation,
            ),
            TeamIntrinsicTool::ReadInbox => self.team_spec(
                "Read and drain any currently pending mailbox messages for this agent.",
                json!({
                    "type": "object",
                    "properties": {}
                }),
                ToolExecutionCategory::ExclusivePersistentMutation,
            ),
            TeamIntrinsicTool::Broadcast => self.team_spec(
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
                ToolExecutionCategory::ExclusivePersistentMutation,
            ),
            TeamIntrinsicTool::Request => self.team_spec(
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
                ToolExecutionCategory::ExclusivePersistentMutation,
            ),
            TeamIntrinsicTool::Respond => self.team_spec(
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
                ToolExecutionCategory::ExclusivePersistentMutation,
            ),
            TeamIntrinsicTool::ListRequests => self.team_spec(
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
                ToolExecutionCategory::ReadOnlyParallel,
            ),
        }
    }
}
