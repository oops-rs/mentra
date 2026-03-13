use async_trait::async_trait;
use serde_json::{Value, json};

use crate::tool::{
    ExecutableTool, ParallelToolContext, ToolCapability, ToolDurability, ToolExecutionMode,
    ToolResult, ToolSideEffectLevel, ToolSpec, context::RuntimeContext,
};

pub struct ShellTool;
pub struct BackgroundRunTool;
pub struct CheckBackgroundTool;
pub struct LoadSkillTool;

struct ShellCommandOutput<'a> {
    command: String,
    working_directory: Option<&'a str>,
    justification: Option<String>,
    requested_timeout: Option<std::time::Duration>,
}

fn parse_command_input<'a>(input: &'a Value) -> Result<ShellCommandOutput<'a>, String> {
    let command = input
        .get("command")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "Command is required".to_string())?
        .to_string();
    let working_directory = input
        .get("workingDirectory")
        .and_then(|value| value.as_str());
    let justification = input
        .get("justification")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    let requested_timeout = input
        .get("timeoutMs")
        .and_then(|value| value.as_u64())
        .map(std::time::Duration::from_millis);

    Ok(ShellCommandOutput {
        command,
        working_directory,
        justification,
        requested_timeout,
    })
}

async fn execute_shell<C>(ctx: &C, input: Value) -> ToolResult
where
    C: RuntimeContext + Sync,
{
    let ShellCommandOutput {
        command,
        working_directory,
        justification,
        requested_timeout,
    } = parse_command_input(&input)?;
    let working_directory = ctx.resolve_working_directory(working_directory)?;
    let output = ctx
        .execute_shell_command(command, justification, requested_timeout, working_directory)
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

async fn execute_background_run<C>(ctx: &C, input: Value) -> ToolResult
where
    C: RuntimeContext + Sync,
{
    let ShellCommandOutput {
        command,
        working_directory,
        justification,
        ..
    } = parse_command_input(&input)?;
    let working_directory = ctx.resolve_working_directory(working_directory)?;
    let task = ctx.start_background_task(command, justification, None, working_directory)?;
    Ok(format!(
        "Started background task {} in {} for `{}`",
        task.id,
        task.cwd.display(),
        task.command
    ))
}

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

    fn execution_mode(&self, _input: &Value) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    async fn execute(&self, ctx: ParallelToolContext, input: Value) -> ToolResult {
        execute_shell(&ctx, input).await
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

    fn execution_mode(&self, _input: &Value) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    async fn execute(&self, ctx: ParallelToolContext, input: Value) -> ToolResult {
        execute_background_run(&ctx, input).await
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

    fn execution_mode(&self, _input: &Value) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    async fn execute(&self, ctx: ParallelToolContext, input: Value) -> ToolResult {
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

    fn execution_mode(&self, _input: &Value) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    async fn execute(&self, ctx: ParallelToolContext, input: Value) -> ToolResult {
        let name = input
            .get("name")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "Skill name is required".to_string())?;

        ctx.load_skill(name)
    }
}
