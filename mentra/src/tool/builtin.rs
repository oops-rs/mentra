#[path = "files.rs"]
mod files;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::tool::{
    ExecutableTool, ToolCapability, ToolContext, ToolDurability, ToolResult, ToolSideEffectLevel,
    ToolSpec,
};

pub use files::FilesTool;

pub struct ShellTool;
pub struct BackgroundRunTool;
pub struct CheckBackgroundTool;
pub struct LoadSkillTool;

#[async_trait]
impl ExecutableTool for ShellTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "shell".to_string(),
            description: Some("Execute a single local shell command.".into()),
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
                    },
                    "timeoutMs": {
                        "type": "integer",
                        "description": "Optional timeout override in milliseconds"
                    },
                    "justification": {
                        "type": "string",
                        "description": "Optional explanation surfaced when approval is required"
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
        let justification = input
            .get("justification")
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned);
        let requested_timeout = input
            .get("timeoutMs")
            .and_then(|value| value.as_u64())
            .map(std::time::Duration::from_millis);

        let output = ctx
            .execute_shell_command(
                command.to_string(),
                justification,
                requested_timeout,
                working_directory,
            )
            .await?;

        if output.success() {
            if !output.stdout.is_empty() {
                Ok(output.stdout)
            } else {
                Ok(output.stderr)
            }
        } else {
            let message = if !output.stderr.trim().is_empty() {
                output.stderr
            } else if !output.stdout.trim().is_empty() {
                output.stdout
            } else if output.timed_out {
                "Command timed out after the configured limit".to_string()
            } else {
                format!(
                    "Command exited with status {}",
                    output
                        .status_code
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "unknown".to_string())
                )
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
                "Start a shell command in the background and return a task ID immediately.".into(),
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
                    },
                    "justification": {
                        "type": "string",
                        "description": "Optional explanation surfaced when approval is required"
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
        let justification = input
            .get("justification")
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned);

        let task =
            ctx.start_background_task(command.to_string(), justification, None, working_directory)?;
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
