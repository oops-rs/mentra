use serde_json::json;

use crate::tool::{
    RuntimeToolDescriptor, ToolApprovalCategory, ToolCapability, ToolDurability,
    ToolExecutionCategory, ToolSideEffectLevel,
    internal::{RuntimeDescriptorParts, build_runtime_descriptor},
};

use super::TaskIntrinsicTool;

pub(super) fn task_intrinsic_descriptor(tool: TaskIntrinsicTool) -> RuntimeToolDescriptor {
    let description = match tool {
        TaskIntrinsicTool::Create => {
            "Lead-oriented project planning tool. Create a persisted task."
        }
        TaskIntrinsicTool::Claim => {
            "Claim a ready unowned persisted task for the current teammate."
        }
        TaskIntrinsicTool::Update => {
            "Lead-oriented project planning tool. Update a persisted task and its dependency edges."
        }
        TaskIntrinsicTool::List => "List persisted tasks grouped by readiness.",
        TaskIntrinsicTool::Get => "Get one persisted task by ID.",
    };

    let input_schema = match tool {
        TaskIntrinsicTool::Create => json!({
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
                "workingDirectory": {
                    "type": ["string", "null"],
                    "description": "Optional working directory hint for shell-based work"
                },
                "blockedBy": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "Task IDs that must finish before this task is ready"
                }
            },
            "required": ["subject"]
        }),
        TaskIntrinsicTool::Claim => json!({
            "type": "object",
            "properties": {
                "taskId": {
                    "type": "integer",
                    "description": "Optional explicit task identifier to claim"
                }
            }
        }),
        TaskIntrinsicTool::Update => json!({
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
                "workingDirectory": {
                    "type": ["string", "null"],
                    "description": "Updated working directory hint for shell-based work; pass null to clear it"
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
        TaskIntrinsicTool::List => json!({
            "type": "object",
            "properties": {}
        }),
        TaskIntrinsicTool::Get => json!({
            "type": "object",
            "properties": {
                "taskId": {
                    "type": "integer",
                    "description": "Stable identifier for the task"
                }
            },
            "required": ["taskId"]
        }),
    };

    build_runtime_descriptor(RuntimeDescriptorParts {
        name: tool.to_string(),
        description: description.to_string(),
        input_schema,
        capabilities: vec![ToolCapability::TaskMutation],
        side_effect_level: ToolSideEffectLevel::LocalState,
        durability: ToolDurability::Persistent,
        execution_category: ToolExecutionCategory::ExclusivePersistentMutation,
        approval_category: ToolApprovalCategory::Default,
    })
}
