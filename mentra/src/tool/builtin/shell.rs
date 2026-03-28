use async_trait::async_trait;
use serde_json::{json, Value};

use crate::tool::{
    ParallelToolContext, RuntimeToolDescriptor, ToolApprovalCategory, ToolAuthorizationPreview,
    ToolCapability, ToolDefinition, ToolDurability, ToolExecutionCategory, ToolExecutor,
    ToolResult, ToolSideEffectLevel, context::RuntimeContext,
};

pub struct ShellTool;
pub struct BackgroundRunTool;

struct ShellCommandInput<'a> {
    command: String,
    working_directory: Option<&'a str>,
    justification: Option<String>,
    requested_timeout: Option<std::time::Duration>,
}

fn parse_shell_command_input<'a>(input: &'a Value) -> Result<ShellCommandInput<'a>, String> {
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

    Ok(ShellCommandInput {
        command,
        working_directory,
        justification,
        requested_timeout,
    })
}

fn shell_input_schema(include_timeout: bool) -> Value {
    let mut properties = serde_json::Map::from_iter([
        (
            "command".to_string(),
            json!({
                "type": "string",
                "description": "Shell command to execute"
            }),
        ),
        (
            "workingDirectory".to_string(),
            json!({
                "type": "string",
                "description": "Optional directory to run inside"
            }),
        ),
        (
            "justification".to_string(),
            json!({
                "type": "string",
                "description": "Optional explanation surfaced when approval is required"
            }),
        ),
    ]);
    if include_timeout {
        properties.insert(
            "timeoutMs".to_string(),
            json!({
                "type": "integer",
                "description": "Optional timeout override in milliseconds"
            }),
        );
    }
    Value::Object(serde_json::Map::from_iter([
        ("type".to_string(), json!("object")),
        ("properties".to_string(), Value::Object(properties)),
        ("required".to_string(), json!(["command"])),
    ]))
}

fn shell_descriptor(background: bool) -> RuntimeToolDescriptor {
    let (name, description, capabilities, durability, execution_category, approval_category) =
        if background {
            (
                "background_run",
                "Start a shell command in the background and return a task ID immediately.",
                vec![ToolCapability::BackgroundExec, ToolCapability::FilesystemWrite],
                ToolDurability::Persistent,
                ToolExecutionCategory::BackgroundJob,
                ToolApprovalCategory::Background,
            )
        } else {
            (
                "shell",
                "Execute a single local shell command.",
                vec![ToolCapability::ProcessExec, ToolCapability::FilesystemWrite],
                ToolDurability::Ephemeral,
                ToolExecutionCategory::ExclusiveLocalMutation,
                ToolApprovalCategory::Process,
            )
        };

    RuntimeToolDescriptor::builder(name)
        .description(description)
        .input_schema(shell_input_schema(!background))
        .capabilities(capabilities)
        .side_effect_level(ToolSideEffectLevel::Process)
        .durability(durability)
        .execution_category(execution_category)
        .approval_category(approval_category)
        .build()
}

fn shell_authorization_preview(
    ctx: &ParallelToolContext,
    input: &Value,
    background: bool,
    descriptor: RuntimeToolDescriptor,
) -> Result<ToolAuthorizationPreview, String> {
    let ShellCommandInput {
        command,
        working_directory,
        justification,
        requested_timeout,
    } = parse_shell_command_input(input)?;
    let working_directory = ctx.resolve_working_directory(working_directory)?;

    Ok(ToolAuthorizationPreview {
        working_directory: working_directory.clone(),
        capabilities: descriptor.capabilities,
        side_effect_level: descriptor.side_effect_level,
        durability: descriptor.durability,
        execution_category: descriptor.execution_category,
        approval_category: descriptor.approval_category,
        raw_input: input.clone(),
        structured_input: json!({
            "kind": if background { "background_run" } else { "shell" },
            "command": command,
            "working_directory": working_directory,
            "timeout_ms": requested_timeout.map(|timeout| timeout.as_millis()),
            "justification": justification,
            "background": background,
        }),
    })
}

async fn execute_shell_command<C>(ctx: &C, input: Value) -> ToolResult
where
    C: RuntimeContext + Sync,
{
    let ShellCommandInput {
        command,
        working_directory,
        justification,
        requested_timeout,
    } = parse_shell_command_input(&input)?;
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

async fn execute_background_command<C>(ctx: &C, input: Value) -> ToolResult
where
    C: RuntimeContext + Sync,
{
    let ShellCommandInput {
        command,
        working_directory,
        justification,
        ..
    } = parse_shell_command_input(&input)?;
    let working_directory = ctx.resolve_working_directory(working_directory)?;
    let task = ctx.start_background_task(command, justification, None, working_directory)?;
    Ok(format!(
        "Started background task {} in {} for `{}`",
        task.id,
        task.cwd.display(),
        task.command
    ))
}

impl ToolDefinition for ShellTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        shell_descriptor(false)
    }
}

#[async_trait]
impl ToolExecutor for ShellTool {
    fn authorization_preview(
        &self,
        ctx: &ParallelToolContext,
        input: &Value,
    ) -> Result<ToolAuthorizationPreview, String> {
        shell_authorization_preview(ctx, input, false, self.descriptor())
    }

    async fn execute_mut(&self, ctx: crate::tool::ToolContext<'_>, input: Value) -> ToolResult {
        execute_shell_command(&ctx, input).await
    }
}

impl ToolDefinition for BackgroundRunTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        shell_descriptor(true)
    }
}

#[async_trait]
impl ToolExecutor for BackgroundRunTool {
    fn authorization_preview(
        &self,
        ctx: &ParallelToolContext,
        input: &Value,
    ) -> Result<ToolAuthorizationPreview, String> {
        shell_authorization_preview(ctx, input, true, self.descriptor())
    }

    async fn execute_mut(&self, ctx: crate::tool::ToolContext<'_>, input: Value) -> ToolResult {
        execute_background_command(&ctx, input).await
    }
}
