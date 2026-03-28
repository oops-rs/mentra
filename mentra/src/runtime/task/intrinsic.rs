use async_trait::async_trait;
use serde_json::json;
use strum::{Display, VariantArray};

use crate::{
    ContentBlock,
    runtime::Agent,
    tool::{
        ToolCall, ToolCapability, ToolContext, ToolDefinition, ToolDurability,
        ToolExecutionCategory, ToolExecutor, ToolResult, ToolSideEffectLevel,
        RuntimeToolDescriptor, ToolApprovalCategory,
    },
};

#[derive(Clone, Copy, Display, VariantArray)]
#[strum(prefix = "task_")]
#[strum(serialize_all = "snake_case")]
pub enum TaskIntrinsicTool {
    Create,
    Claim,
    Update,
    List,
    Get,
}

impl TaskIntrinsicTool {
    fn task_spec(&self, description: &str, input_schema: serde_json::Value) -> RuntimeToolDescriptor {
        RuntimeToolDescriptor::builder(self.to_string())
            .description(description)
            .input_schema(input_schema)
            .capability(ToolCapability::TaskMutation)
            .side_effect_level(ToolSideEffectLevel::LocalState)
            .durability(ToolDurability::Persistent)
            .execution_category(ToolExecutionCategory::ExclusivePersistentMutation)
            .approval_category(ToolApprovalCategory::Default)
            .build()
    }
}

impl ToolDefinition for TaskIntrinsicTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        match self {
            Self::Create => self.task_spec(
                "Lead-oriented project planning tool. Create a persisted task.",
                json!({
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
            ),
            Self::Claim => self.task_spec(
                "Claim a ready unowned persisted task for the current teammate.",
                json!({
                    "type": "object",
                    "properties": {
                        "taskId": {
                            "type": "integer",
                            "description": "Optional explicit task identifier to claim"
                        }
                    }
                }),
            ),
            Self::Update => self.task_spec(
                "Lead-oriented project planning tool. Update a persisted task and its dependency edges.",
                json!({
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
            ),
            Self::List => self.task_spec(
                "List persisted tasks grouped by readiness.",
                json!({
                    "type": "object",
                    "properties": {}
                }),
            ),
            Self::Get => self.task_spec(
                "Get one persisted task by ID.",
                json!({
                    "type": "object",
                    "properties": {
                        "taskId": {
                            "type": "integer",
                            "description": "Stable identifier for the task"
                        }
                    },
                    "required": ["taskId"]
                }),
            ),
        }
    }
}

#[async_trait]
impl ToolExecutor for TaskIntrinsicTool {
    async fn execute_mut(&self, ctx: ToolContext<'_>, input: serde_json::Value) -> ToolResult {
        let call = ToolCall {
            id: ctx.tool_call_id.clone(),
            name: self.descriptor().provider.name,
            input,
        };
        let Some(result) = execute_intrinsic(ctx.agent, call) else {
            return Err("Task intrinsic is not available".to_string());
        };
        content_block_to_result(result)
    }
}

pub(crate) fn execute_intrinsic(agent: &mut Agent, call: ToolCall) -> Option<ContentBlock> {
    let tool = TaskIntrinsicTool::VARIANTS
        .iter()
        .find(|tool| tool.descriptor().provider.name == call.name)?;

    let output = agent.execute_task_mutation(tool, call.input);

    Some(match output {
        Ok(content) => match agent.refresh_tasks_from_disk() {
            Ok(()) => ContentBlock::ToolResult {
                tool_use_id: call.id,
                content: content.into(),
                is_error: false,
            },
            Err(error) => ContentBlock::ToolResult {
                tool_use_id: call.id,
                content: format!("Task refresh failed: {error}").into(),
                is_error: true,
            },
        },
        Err(content) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: content.into(),
            is_error: true,
        },
    })
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
        _ => Err("Task intrinsic returned an unexpected content block".to_string()),
    }
}
