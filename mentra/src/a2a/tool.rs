//! The bridge that makes one remote A2A agent a tool a model can call.
//!
//! # The call
//!
//! The tool takes `{ "message"?, "task_id"?, "context_id"? }`:
//!
//! | input | what it does |
//! |---|---|
//! | `message` | starts a new task, in `context_id`'s conversation when given |
//! | `task_id` + `message` | answers a task that is waiting for input |
//! | `task_id` alone | resumes waiting on a task, sending nothing |
//!
//! A message is submitted with `returnImmediately`, so the agent answers as
//! soon as the task exists, and the task is then polled with `GetTask` until
//! it reaches a terminal state, pauses for input or authorization, or the
//! call's wait bound ([`A2aLimits::max_wait`]) passes. A task still working at
//! the bound is handed back with its ids and the instruction to call again
//! with `task_id` alone — so a long task costs the model more calls, never a
//! second task.
//!
//! # Cancellation
//!
//! The poll checks the run's cancellation and deadline between polls. A
//! cancelled run asks the remote agent to cancel the task before the call
//! returns; a run asked to *stop* gracefully hands the task back still
//! running, with its ids, because a stop keeps the transcript and a later turn
//! may want to resume it. A call whose future is dropped mid-poll — the
//! runtime's own execution timeout, or a host aborting the run — sends the
//! cancel from a detached task, best-effort, because `Drop` cannot await.
//!
//! [`A2aLimits::max_wait`]: crate::a2a::A2aLimits::max_wait

mod render;
#[cfg(test)]
mod tests;

use std::sync::Arc;
// The wait loop moved to `exchange`; the tests still read their bounds as
// `Duration`s through `use super::*`.
#[cfg(test)]
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::client::A2aClient;
use super::exchange::{A2aExchange, A2aExchangeError, A2aRequest, A2aSend, exchange, state_name};
use super::name::a2a_tool_name;
use crate::runtime::RunOptions;
use crate::tool::{
    ParallelToolContext, RuntimeToolDescriptor, ToolApprovalCategory, ToolCapability, ToolContext,
    ToolDefinition, ToolDurability, ToolExecutionCategory, ToolExecutor, ToolOutput, ToolResult,
    ToolSideEffectLevel,
};
use a2a_lf::{Task, TaskState};
use render::Frame;

/// One remote agent, bridged into the tool roster as `a2a__{agent}`.
///
/// Built from a connected [`A2aClient`]; the tool's name, description and
/// bounds all come from it, so a host registers it as it would any other
/// [`ExecutableTool`](crate::tool::ExecutableTool).
///
/// # Classification
///
/// - **Side effects: [`External`](ToolSideEffectLevel::External).** A message
///   hands work to a system this process does not control, exactly as an MCP
///   tool call does, so an approval policy keyed on side effects treats both
///   alike.
/// - **Execution and approval category:
///   [`Delegation`](ToolExecutionCategory::Delegation).** The call hands a
///   task to another agent and waits on it, which is what the built-in `task`
///   subagent tool declares. The scheduler runs it exclusively, never in a
///   parallel batch: two calls racing the same remote task would each poll it
///   and could each answer it.
/// - **Durability: [`Ephemeral`](ToolDurability::Ephemeral).** Replaying a
///   call would send its message again and start a second task.
pub struct A2aAgentTool {
    client: Arc<A2aClient>,
    tool_name: String,
    description: String,
}

impl A2aAgentTool {
    /// Bridges a connected agent.
    pub fn new(client: Arc<A2aClient>) -> Self {
        let tool_name = a2a_tool_name(client.name());
        let description = render::description(client.card(), client.name(), &tool_name);
        Self {
            client,
            tool_name,
            description,
        }
    }

    /// The client this tool speaks through.
    pub fn client(&self) -> &Arc<A2aClient> {
        &self.client
    }

    /// The tool's name, `a2a__{agent}`.
    pub fn name(&self) -> &str {
        &self.tool_name
    }

    fn frame(&self) -> Frame<'_> {
        Frame {
            agent: self.client.name(),
            tool: &self.tool_name,
            max_output_bytes: self.client.limits().max_output_bytes,
        }
    }

    /// Runs one call: submit or resume, then wait for the task to settle.
    async fn run(
        &self,
        call: Call,
        options: &RunOptions,
        progress: &(dyn Fn(String) + Send + Sync),
    ) -> Result<ToolOutput, String> {
        let request = match call {
            Call::Send {
                text,
                task_id,
                context_id,
            } => {
                let mut send = A2aSend::new(text);
                send.task_id = task_id;
                send.context_id = context_id;
                A2aRequest::Send(send)
            }
            Call::Resume { task_id } => A2aRequest::resume(task_id),
        };
        let frame = self.frame();
        match exchange(&self.client, request, options, progress).await {
            Ok(A2aExchange::Reply(reply)) => {
                let details = json!({ "context_id": reply.context_id });
                Ok(ToolOutput::text(render::direct_reply(&frame, &reply)).with_details(details))
            }
            Ok(A2aExchange::Settled(task)) => finished(render::settled(&frame, &task), &task),
            Ok(A2aExchange::StillWorking { task, reason }) => {
                Ok(output(render::still_working(&frame, &task, reason), &task))
            }
            Err(A2aExchangeError::Send(error)) => Err(self.failure(error)),
            Err(A2aExchangeError::Poll { task_id, error }) => {
                Err(render::poll_failed(&frame, &task_id, &error.to_string()))
            }
            Err(A2aExchangeError::Abandoned { task_id, canceled }) => Err(render::abandoned(
                &frame,
                &task_id,
                canceled.map_err(|error| error.to_string()),
            )),
        }
    }

    fn failure(&self, error: super::A2aError) -> String {
        format!(
            "A2A call to remote agent `{}` failed: {error}",
            self.client.name()
        )
    }
}

impl std::fmt::Debug for A2aAgentTool {
    /// Renders the bridged identity without reaching into the client.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A2aAgentTool")
            .field("name", &self.tool_name)
            .finish_non_exhaustive()
    }
}

impl ToolDefinition for A2aAgentTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        RuntimeToolDescriptor::builder(&self.tool_name)
            .description(&self.description)
            .input_schema(input_schema())
            .capabilities([
                ToolCapability::Delegation,
                ToolCapability::Custom(format!("a2a:{}", self.client.name())),
            ])
            .side_effect_level(ToolSideEffectLevel::External)
            .durability(ToolDurability::Ephemeral)
            .execution_category(ToolExecutionCategory::Delegation)
            .approval_category(ToolApprovalCategory::Delegation)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for A2aAgentTool {
    async fn execute(&self, ctx: ParallelToolContext, input: Value) -> ToolResult {
        self.execute_output(ctx, input)
            .await
            .map(|output| output.content.to_string())
    }

    async fn execute_mut(&self, ctx: ToolContext<'_>, input: Value) -> ToolResult {
        self.execute(ctx.into(), input).await
    }

    /// Carries the task's ids and state as details, so a host can record them
    /// without parsing the text the model reads.
    async fn execute_output(
        &self,
        ctx: ParallelToolContext,
        input: Value,
    ) -> Result<ToolOutput, String> {
        let call = Call::parse(input)?;
        let progress = |text: String| ctx.emit_progress(text);
        self.run(call, &ctx.run_options, &progress).await
    }

    async fn execute_mut_output(
        &self,
        ctx: ToolContext<'_>,
        input: Value,
    ) -> Result<ToolOutput, String> {
        self.execute_output(ctx.into(), input).await
    }
}

/// What one call asks for, after its input is checked.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    /// Send a message: a new task, or an answer to a waiting one.
    Send {
        text: String,
        task_id: Option<String>,
        context_id: Option<String>,
    },
    /// Wait on an existing task without sending anything.
    Resume { task_id: String },
}

/// The tool's input as the model sends it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    context_id: Option<String>,
}

impl Call {
    fn parse(input: Value) -> Result<Self, String> {
        let input: Input = serde_json::from_value(input).map_err(|error| {
            format!("invalid input: {error}; expected {{message?, task_id?, context_id?}}")
        })?;
        let present = |value: Option<String>, field: &str| match value {
            Some(value) if value.trim().is_empty() => {
                Err(format!("invalid input: `{field}` must not be empty"))
            }
            other => Ok(other),
        };
        let message = present(input.message, "message")?;
        let task_id = present(input.task_id, "task_id")?;
        let context_id = present(input.context_id, "context_id")?;

        match (message, task_id) {
            (Some(text), task_id) => Ok(Self::Send {
                text,
                task_id,
                context_id,
            }),
            (None, Some(task_id)) if context_id.is_none() => Ok(Self::Resume { task_id }),
            (None, Some(_)) => {
                Err("invalid input: `context_id` only applies when sending a `message`".to_string())
            }
            (None, None) => Err(
                "invalid input: pass `message` to send, or `task_id` alone to resume waiting"
                    .to_string(),
            ),
        }
    }
}

fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "message": {
                "type": "string",
                "description": "Text to send to the remote agent. Alone, it starts a new task; \
                                with task_id, it answers a task that is waiting for input."
            },
            "task_id": {
                "type": "string",
                "description": "A task id from an earlier reply. Alone, it resumes waiting on \
                                that task without sending anything."
            },
            "context_id": {
                "type": "string",
                "description": "A context id from an earlier reply, to start the new task in \
                                that conversation."
            }
        },
        "additionalProperties": false
    })
}

/// A settled task's result: an error for the states that mean the work was
/// not done.
fn finished(text: String, task: &Task) -> Result<ToolOutput, String> {
    match task.status.state {
        TaskState::Failed | TaskState::Rejected | TaskState::Canceled => Err(text),
        _ => Ok(output(text, task)),
    }
}

fn output(text: String, task: &Task) -> ToolOutput {
    ToolOutput::text(text).with_details(json!({
        "task_id": task.id,
        "context_id": task.context_id,
        "state": state_name(&task.status.state),
    }))
}
