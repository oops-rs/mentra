use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::runtime::TODO_TOOL_NAME;
use crate::tool::{ToolContext, ToolHandler, ToolResult, ToolSpec};

pub struct BashTool;
pub struct ReadFileTool;
pub struct TaskTool;
pub struct TodoTool;

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
impl ToolHandler for TodoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: TODO_TOOL_NAME.to_string(),
            description: Some("Update task list. Track progress on multi-step tasks.".into()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {
                                    "type": "string",
                                    "description": "Stable identifier for the todo item"
                                },
                                "text": {
                                    "type": "string",
                                    "description": "Short description of the todo item"
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"],
                                    "description": "Current status of the todo item"
                                }
                            },
                            "required": ["id", "text", "status"]
                        }
                    }
                },
                "required": ["items"]
            }),
        }
    }

    async fn invoke(&self, _ctx: ToolContext, _input: Value) -> ToolResult {
        Err("todo is handled directly by the agent runtime".to_string())
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
