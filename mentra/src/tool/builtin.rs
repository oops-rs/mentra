use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::runtime::{
    TASK_CREATE_TOOL_NAME, TASK_GET_TOOL_NAME, TASK_LIST_TOOL_NAME, TASK_UPDATE_TOOL_NAME,
};
use crate::tool::{ToolContext, ToolHandler, ToolResult, ToolSpec};

pub struct BashTool;
pub struct BackgroundRunTool;
pub struct CheckBackgroundTool;
pub struct CompactTool;
pub struct LoadSkillTool;
pub struct ReadFileTool;
pub struct TaskTool;
pub struct TaskCreateTool;
pub struct TaskGetTool;
pub struct TaskListTool;
pub struct TaskUpdateTool;

#[async_trait]
impl ToolHandler for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".to_string(),
            description: Some("Execute a single local bash command.".into()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command to execute"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn invoke(&self, _ctx: ToolContext, input: Value) -> ToolResult {
        let command = input
            .get("command")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "Command is required".to_string())?;

        let output = Command::new("bash")
            .arg("-c")
            .arg(command)
            .output()
            .await
            .map_err(|error| format!("Failed to execute command: {error}"))?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let message = if stderr.trim().is_empty() {
                format!("Command exited with status {}", output.status)
            } else {
                stderr
            };
            Err(message)
        }
    }
}

#[async_trait]
impl ToolHandler for BackgroundRunTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "background_run".to_string(),
            description: Some(
                "Start a bash command in the background and return a task ID immediately.".into(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command to execute in the background"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn invoke(&self, ctx: ToolContext, input: Value) -> ToolResult {
        let command = input
            .get("command")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "Command is required".to_string())?;

        let task = ctx.start_background_task(command.to_string());
        Ok(format!(
            "Started background task {} for `{}`",
            task.id, task.command
        ))
    }
}

#[async_trait]
impl ToolHandler for CheckBackgroundTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "check_background".to_string(),
            description: Some(
                "Check one background task by ID, or list all background tasks when omitted."
                    .into(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "Optional background task ID to inspect"
                    }
                }
            }),
        }
    }

    async fn invoke(&self, ctx: ToolContext, input: Value) -> ToolResult {
        let task_id = input.get("task_id").and_then(|value| value.as_str());
        ctx.check_background_task(task_id)
    }
}

#[async_trait]
impl ToolHandler for CompactTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "compact".to_string(),
            description: Some("Compress older conversation context into a summary.".into()),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    async fn invoke(&self, _ctx: ToolContext, _input: Value) -> ToolResult {
        Err("compact is handled directly by the agent runtime".to_string())
    }
}

#[async_trait]
impl ToolHandler for ReadFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".to_string(),
            description: Some("Read the first N lines of a file.".into()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to read"
                    },
                    "lines": {
                        "type": "integer",
                        "description": "Maximum number of lines to read. If omitted, read the whole file"
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn invoke(&self, _ctx: ToolContext, input: Value) -> ToolResult {
        let path = input
            .get("path")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "Path is required".to_string())?;
        let max_lines = input
            .get("lines")
            .and_then(|value| value.as_u64())
            .map(|value| value as usize);

        let file = tokio::fs::File::open(path)
            .await
            .map_err(|error| format!("Failed to open file: {error}"))?;
        let mut reader = BufReader::new(file).lines();
        let mut content = Vec::new();

        loop {
            if let Some(limit) = max_lines
                && content.len() >= limit
            {
                break;
            }

            match reader.next_line().await {
                Ok(Some(line)) => content.push(line),
                Ok(None) => break,
                Err(error) => return Err(format!("Failed to read file: {error}")),
            }
        }

        Ok(content.join("\n"))
    }
}

#[async_trait]
impl ToolHandler for LoadSkillTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "load_skill".to_string(),
            description: Some("Load the full body of a named skill when it is relevant.".into()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Name of the skill to load"
                    }
                },
                "required": ["name"]
            }),
        }
    }

    async fn invoke(&self, ctx: ToolContext, input: Value) -> ToolResult {
        let name = input
            .get("name")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "Skill name is required".to_string())?;

        ctx.load_skill(name)
    }
}

#[async_trait]
impl ToolHandler for TaskTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task".to_string(),
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
        }
    }

    async fn invoke(&self, _ctx: ToolContext, _input: Value) -> ToolResult {
        Err("task is handled directly by the agent runtime".to_string())
    }
}

#[async_trait]
impl ToolHandler for TaskCreateTool {
    fn spec(&self) -> ToolSpec {
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
        }
    }

    async fn invoke(&self, _ctx: ToolContext, _input: Value) -> ToolResult {
        Err("task_create is handled directly by the agent runtime".to_string())
    }
}

#[async_trait]
impl ToolHandler for TaskUpdateTool {
    fn spec(&self) -> ToolSpec {
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
        }
    }

    async fn invoke(&self, _ctx: ToolContext, _input: Value) -> ToolResult {
        Err("task_update is handled directly by the agent runtime".to_string())
    }
}

#[async_trait]
impl ToolHandler for TaskListTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: TASK_LIST_TOOL_NAME.to_string(),
            description: Some("List the persisted task graph grouped by readiness.".into()),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    async fn invoke(&self, _ctx: ToolContext, _input: Value) -> ToolResult {
        Err("task_list is handled directly by the agent runtime".to_string())
    }
}

#[async_trait]
impl ToolHandler for TaskGetTool {
    fn spec(&self) -> ToolSpec {
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
        }
    }

    async fn invoke(&self, _ctx: ToolContext, _input: Value) -> ToolResult {
        Err("task_get is handled directly by the agent runtime".to_string())
    }
}
