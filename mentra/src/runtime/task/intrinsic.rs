use async_trait::async_trait;
use serde_json::json;

use crate::{
    ContentBlock,
    runtime::{
        Agent, TASK_CREATE_TOOL_NAME, TASK_GET_TOOL_NAME, TASK_LIST_TOOL_NAME,
        TASK_UPDATE_TOOL_NAME, task,
    },
    tool::{
        ExecutableTool, ToolCall, ToolCapability, ToolContext, ToolDurability, ToolResult,
        ToolSideEffectLevel, ToolSpec,
    },
};

fn task_spec(name: &str, description: &str, input_schema: serde_json::Value) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: Some(description.to_string()),
        input_schema,
        capabilities: vec![ToolCapability::TaskMutation],
        side_effect_level: ToolSideEffectLevel::LocalState,
        durability: ToolDurability::Persistent,
    }
}

pub(crate) fn intrinsic_specs() -> Vec<ToolSpec> {
    vec![
        task_spec(
            TASK_CREATE_TOOL_NAME,
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
        task_spec(
            task::TASK_CLAIM_TOOL_NAME,
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
        task_spec(
            TASK_UPDATE_TOOL_NAME,
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
        task_spec(
            TASK_LIST_TOOL_NAME,
            "List persisted tasks grouped by readiness.",
            json!({
                "type": "object",
                "properties": {}
            }),
        ),
        task_spec(
            TASK_GET_TOOL_NAME,
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
    ]
}

#[derive(Clone, Copy)]
pub(crate) enum TaskIntrinsicTool {
    Create,
    Claim,
    Update,
    List,
    Get,
}

impl TaskIntrinsicTool {
    fn all() -> [Self; 5] {
        [Self::Create, Self::Claim, Self::Update, Self::List, Self::Get]
    }

    fn spec(self) -> ToolSpec {
        match self {
            Self::Create => intrinsic_specs()[0].clone(),
            Self::Claim => intrinsic_specs()[1].clone(),
            Self::Update => intrinsic_specs()[2].clone(),
            Self::List => intrinsic_specs()[3].clone(),
            Self::Get => intrinsic_specs()[4].clone(),
        }
    }
}

#[async_trait]
impl ExecutableTool for TaskIntrinsicTool {
    fn spec(&self) -> ToolSpec {
        (*self).spec()
    }

    async fn execute(&self, ctx: ToolContext<'_>, input: serde_json::Value) -> ToolResult {
        let call = ToolCall {
            id: ctx.tool_call_id.clone(),
            name: self.spec().name,
            input,
        };
        let Some(result) = execute_intrinsic(ctx.agent, call) else {
            return Err("Task intrinsic is not available".to_string());
        };
        content_block_to_result(result)
    }
}

pub(crate) fn register_tools(registry: &mut crate::tool::ToolRegistry) {
    for tool in TaskIntrinsicTool::all() {
        registry.register_tool(tool);
    }
}

pub(crate) fn execute_intrinsic(agent: &mut Agent, call: ToolCall) -> Option<ContentBlock> {
    if !task::is_task_tool(&call.name) {
        return None;
    }
    let output = agent.execute_task_mutation(&call.name, call.input);

    Some(match output {
        Ok(content) => match agent.refresh_tasks_from_disk() {
            Ok(()) => ContentBlock::ToolResult {
                tool_use_id: call.id,
                content,
                is_error: false,
            },
            Err(error) => ContentBlock::ToolResult {
                tool_use_id: call.id,
                content: format!("Task refresh failed: {error:?}"),
                is_error: true,
            },
        },
        Err(content) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content,
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
                Err(content)
            } else {
                Ok(content)
            }
        }
        _ => Err("Task intrinsic returned an unexpected content block".to_string()),
    }
}
