use async_trait::async_trait;
use serde_json::{Value, json};

use crate::tool::{
    ExecutableTool, ToolCapability, ToolContext, ToolDurability, ToolResult, ToolSideEffectLevel,
    ToolSpec,
};

pub struct BashTool;
pub struct BackgroundRunTool;
pub struct CheckBackgroundTool;
pub struct LoadSkillTool;
pub struct ReadFileTool;

#[async_trait]
impl ExecutableTool for BashTool {
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
                    },
                    "workingDirectory": {
                        "type": "string",
                        "description": "Optional directory to run inside"
                    }
                },
                "required": ["command"]
            }),
            capabilities: vec![ToolCapability::ProcessExec, ToolCapability::FilesystemWrite],
            side_effect_level: ToolSideEffectLevel::Process,
            durability: ToolDurability::Ephemeral,
        }
    }

    async fn execute(&self, ctx: ToolContext<'_>, input: Value) -> ToolResult {
        let command = input
            .get("command")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "Command is required".to_string())?;

        let working_directory = input
            .get("workingDirectory")
            .and_then(|value| value.as_str());
        let working_directory = ctx.resolve_working_directory(working_directory)?;

        let output = ctx
            .execute_shell_command(command.to_string(), working_directory)
            .await?;

        if output.success() {
            Ok(output.stdout)
        } else {
            let message = if output.stderr.trim().is_empty() {
                format!(
                    "Command exited with status {}",
                    output
                        .status_code
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "unknown".to_string())
                )
            } else {
                output.stderr
            };
            Err(message)
        }
    }
}

#[async_trait]
impl ExecutableTool for BackgroundRunTool {
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
                    },
                    "workingDirectory": {
                        "type": "string",
                        "description": "Optional directory to run inside"
                    }
                },
                "required": ["command"]
            }),
            capabilities: vec![
                ToolCapability::BackgroundExec,
                ToolCapability::FilesystemWrite,
            ],
            side_effect_level: ToolSideEffectLevel::Process,
            durability: ToolDurability::Persistent,
        }
    }

    async fn execute(&self, ctx: ToolContext<'_>, input: Value) -> ToolResult {
        let command = input
            .get("command")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "Command is required".to_string())?;
        let working_directory = input
            .get("workingDirectory")
            .and_then(|value| value.as_str());
        let working_directory = ctx.resolve_working_directory(working_directory)?;

        let task = ctx.start_background_task(command.to_string(), working_directory)?;
        Ok(format!(
            "Started background task {} in {} for `{}`",
            task.id,
            task.cwd.display(),
            task.command
        ))
    }
}

#[async_trait]
impl ExecutableTool for CheckBackgroundTool {
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
            capabilities: vec![ToolCapability::ReadOnly],
            side_effect_level: ToolSideEffectLevel::None,
            durability: ToolDurability::ReplaySafe,
        }
    }

    async fn execute(&self, ctx: ToolContext<'_>, input: Value) -> ToolResult {
        let task_id = input.get("task_id").and_then(|value| value.as_str());
        ctx.check_background_task(task_id)
    }
}

#[async_trait]
impl ExecutableTool for ReadFileTool {
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
            capabilities: vec![ToolCapability::ReadOnly, ToolCapability::FilesystemRead],
            side_effect_level: ToolSideEffectLevel::None,
            durability: ToolDurability::ReplaySafe,
        }
    }

    async fn execute(&self, _ctx: ToolContext<'_>, input: Value) -> ToolResult {
        let path = input
            .get("path")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "Path is required".to_string())?;
        let max_lines = input
            .get("lines")
            .and_then(|value| value.as_u64())
            .map(|value| value as usize);

        _ctx.read_file(path, max_lines).await
    }
}

#[async_trait]
impl ExecutableTool for LoadSkillTool {
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
            capabilities: vec![ToolCapability::SkillLoad, ToolCapability::ReadOnly],
            side_effect_level: ToolSideEffectLevel::None,
            durability: ToolDurability::ReplaySafe,
        }
    }

    async fn execute(&self, ctx: ToolContext<'_>, input: Value) -> ToolResult {
        let name = input
            .get("name")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "Skill name is required".to_string())?;

        ctx.load_skill(name)
    }
}
