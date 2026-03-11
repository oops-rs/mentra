use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::tool::{ToolContext, ToolHandler, ToolResult, ToolSpec};

pub struct BashTool;
pub struct BackgroundRunTool;
pub struct CheckBackgroundTool;
pub struct LoadSkillTool;
pub struct ReadFileTool;

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
