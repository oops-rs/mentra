//! One request to a remote agent, waited on until it settles — as data.
//!
//! [`exchange`] is the loop behind [`A2aAgentTool`](crate::a2a::A2aAgentTool)
//! without the tool's words: it submits or resumes, waits on the task, and hands
//! back what happened rather than text a model reads. A host that offers a
//! remote agent through a tool of its own — a different name, different
//! guidance about what to call next — drives the same loop and writes its own
//! text, with [`remote_output`](crate::a2a::remote_output) and
//! [`remote_reply`](crate::a2a::remote_reply) framing the agent's words the
//! same way the bridged tool does.
//!
//! # The loop
//!
//! A message is submitted with `returnImmediately`, so the agent answers as
//! soon as the task exists — the moment a host records the task id, between
//! [`submit`] and [`settle`]. The task is then waited on until it
//! [settles](is_settled), the client's wait bound
//! ([`A2aLimits::max_wait`](crate::a2a::A2aLimits::max_wait)) passes, or the
//! run ends:
//!
//! - when the agent's card declares `capabilities.streaming`, by following
//!   the task with `SubscribeToTask`, applying its status and artifact events
//!   and reporting each status message as progress;
//! - otherwise, and whenever that stream is refused, drops, falls idle or
//!   fails, by polling `GetTask` from the task as it last stood.
//!
//! Neither path sends the message again. A cancelled run asks the remote
//! agent to cancel the task before the exchange returns; a run asked to
//! *stop* hands the task back still running. A future dropped mid-wait sends
//! the cancel from a detached task, best-effort, because `Drop` cannot await.

mod follow;
#[cfg(test)]
mod tests;
mod wait;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use a2a_lf::{Message, Part, Role, SendMessageConfiguration, SendMessageResponse, Task, TaskState};
use serde_json::Value;
use tokio::time::Instant;

use super::client::A2aClient;
use super::error::A2aError;
use super::output::remote_line;
use crate::runtime::RunOptions;
use follow::{Followed, follow};
use wait::{Wake, wait};

/// Bound on a task id in a progress line.
const MAX_PROGRESS_ID_BYTES: usize = 128;

/// What one exchange asks of the remote agent.
///
/// Non-exhaustive so that a way of asking added later is not a breaking
/// change; build one with [`A2aRequest::send`], [`A2aRequest::resume`], or
/// from an [`A2aSend`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum A2aRequest {
    /// Send a message.
    Send(A2aSend),
    /// Wait on an existing task without sending anything.
    Resume { task_id: String },
}

impl A2aRequest {
    /// Sends `text` as a new task.
    pub fn send(text: impl Into<String>) -> Self {
        Self::Send(A2aSend::new(text))
    }

    /// Waits on `task_id` without sending anything.
    pub fn resume(task_id: impl Into<String>) -> Self {
        Self::Resume {
            task_id: task_id.into(),
        }
    }
}

impl From<A2aSend> for A2aRequest {
    fn from(send: A2aSend) -> Self {
        Self::Send(send)
    }
}

/// A message to send: a new task — in a conversation when it names a
/// context — or, when it names a task, an answer to a task waiting for input.
///
/// Its fields are readable, and it is built with [`new`](Self::new) and the
/// `with_*` methods, so a field added later — as `metadata` and `extensions`
/// were — does not break a host that builds one.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct A2aSend {
    /// The message's text.
    pub text: String,
    /// The task this message answers, when it answers one.
    pub task_id: Option<String>,
    /// The conversation a new task joins.
    pub context_id: Option<String>,
    /// The message's `metadata`: where an extension puts what it carries,
    /// under its own URI.
    pub metadata: HashMap<String, Value>,
    /// The URIs of the extensions this message uses: listed in the message's
    /// `extensions`, and activated for the request in `A2A-Extensions`.
    pub extensions: Vec<String>,
}

impl A2aSend {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            task_id: None,
            context_id: None,
            metadata: HashMap::new(),
            extensions: Vec::new(),
        }
    }

    /// Answers `task_id`, a task waiting for input.
    pub fn with_task_id(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = Some(task_id.into());
        self
    }

    /// Starts the task in `context_id`'s conversation.
    pub fn with_context_id(mut self, context_id: impl Into<String>) -> Self {
        self.context_id = Some(context_id.into());
        self
    }

    /// Sets one `metadata` entry, replacing any under the same key.
    pub fn with_metadata(mut self, key: impl Into<String>, value: Value) -> Self {
        self.metadata.insert(key.into(), value);
        self
    }

    /// Uses the extension `uri`, once however often it is added.
    pub fn with_extension(mut self, uri: impl Into<String>) -> Self {
        let uri = uri.into();
        if !self.extensions.contains(&uri) {
            self.extensions.push(uri);
        }
        self
    }

    /// The A2A message this sends.
    fn into_message(self) -> Message {
        let mut message = Message::new(Role::User, vec![Part::text(self.text)]);
        message.task_id = self.task_id;
        message.context_id = self.context_id;
        message.metadata = (!self.metadata.is_empty()).then_some(self.metadata);
        message.extensions = (!self.extensions.is_empty()).then_some(self.extensions);
        message
    }
}

/// What came back once the exchange stopped waiting.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum A2aExchange {
    /// The agent answered with a message and started no task.
    Reply(Message),
    /// The task [settled](is_settled): terminal, or paused for input or
    /// authorization only the caller can give.
    Settled(Task),
    /// The task is still running remotely; resuming it later costs a call,
    /// never a second task.
    StillWorking { task: Task, reason: A2aStillWorking },
}

/// Why an exchange stopped waiting on a task that had not settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum A2aStillWorking {
    /// The client's wait bound passed; the bound is carried for the telling.
    WaitBound(Duration),
    /// The run asked to stop at its next boundary.
    RunStopping,
}

/// Why an exchange produced no outcome.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum A2aExchangeError {
    /// The request itself failed: the message was not accepted, or the task
    /// to resume could not be read. Nothing is known to be running.
    #[error("the request failed: {0}")]
    Send(#[source] A2aError),
    /// Checking on a submitted task failed; it may still be running.
    #[error("checking on task {task_id} failed: {error}")]
    Poll {
        task_id: String,
        #[source]
        error: A2aError,
    },
    /// The run was cancelled while the task was working, and the remote
    /// agent was asked to cancel it — `canceled` says how that went.
    #[error("the run was cancelled while task {task_id} was working{}", cancel_outcome(.canceled))]
    Abandoned {
        task_id: String,
        canceled: Result<(), A2aError>,
    },
}

fn cancel_outcome(canceled: &Result<(), A2aError>) -> String {
    match canceled {
        Ok(()) => "; the remote task was asked to cancel".to_string(),
        Err(error) => format!("; asking the remote task to cancel failed: {error}"),
    }
}

/// What a request produced before anything waited on it.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum A2aSubmitted {
    /// The agent answered with a message; there is no task to wait on.
    Reply(Message),
    /// The task the request created or resumed, as it stood when it came back.
    Task(Task),
}

/// Runs one request against `client` and waits for its task to settle.
///
/// `options` are the bounds of the run this exchange is part of — its
/// cancellation, stop and deadline are checked throughout the wait — and
/// `progress` hears each change of the task's state, and each status message
/// a followed stream carries.
///
/// [`submit`] then [`settle`], with the wait bound counted from before the
/// submit. A host that must record the task before waiting on it — so that a
/// process that dies mid-wait does not send the same work again when it comes
/// back — calls the two halves itself.
pub async fn exchange(
    client: &Arc<A2aClient>,
    request: A2aRequest,
    options: &RunOptions,
    progress: &(dyn Fn(String) + Send + Sync),
) -> Result<A2aExchange, A2aExchangeError> {
    let started = Instant::now();
    match submit(client, request).await? {
        A2aSubmitted::Reply(reply) => Ok(A2aExchange::Reply(reply)),
        A2aSubmitted::Task(task) => settle_from(client, task, started, options, progress).await,
    }
}

/// Sends a request — a message with `returnImmediately`, or a read of an
/// existing task — and returns as soon as the agent answers, without waiting
/// for the task to settle.
///
/// A failure here is [`A2aExchangeError::Send`]: nothing is known to have been
/// created, so there is nothing to cancel or resume.
pub async fn submit(
    client: &Arc<A2aClient>,
    request: A2aRequest,
) -> Result<A2aSubmitted, A2aExchangeError> {
    let task = match request {
        A2aRequest::Send(send) => {
            let message = send.into_message();
            let configuration = SendMessageConfiguration {
                accepted_output_modes: None,
                task_push_notification_config: None,
                history_length: Some(0),
                return_immediately: Some(true),
            };
            match client
                .send_message(message, configuration)
                .await
                .map_err(A2aExchangeError::Send)?
            {
                SendMessageResponse::Message(reply) => return Ok(A2aSubmitted::Reply(reply)),
                SendMessageResponse::Task(task) => task,
            }
        }
        A2aRequest::Resume { task_id } => client
            .get_task(&task_id, Some(0))
            .await
            .map_err(A2aExchangeError::Send)?,
    };
    Ok(A2aSubmitted::Task(task))
}

/// Waits on a task [`submit`] returned — following its stream when the agent
/// offers one, polling otherwise — until it settles, the client's wait bound
/// passes (counted from this call), or the run ends.
///
/// Cancels the task if the run is cancelled, and, best-effort, if this future
/// is dropped mid-wait.
pub async fn settle(
    client: &Arc<A2aClient>,
    task: Task,
    options: &RunOptions,
    progress: &(dyn Fn(String) + Send + Sync),
) -> Result<A2aExchange, A2aExchangeError> {
    settle_from(client, task, Instant::now(), options, progress).await
}

/// Waits on a task until it settles, the wait bound passes, or the run
/// ends: by following its stream when the agent offers one, then by polling.
async fn settle_from(
    client: &Arc<A2aClient>,
    mut task: Task,
    started: Instant,
    options: &RunOptions,
    progress: &(dyn Fn(String) + Send + Sync),
) -> Result<A2aExchange, A2aExchangeError> {
    let limits = client.limits();
    let deadline = started + limits.max_wait;
    let guard = CancelOnDrop::new(client, &task);

    let streams = client.card().capabilities.streaming == Some(true);
    if streams && !is_settled(&task.status.state) {
        // Whatever ended the stream short of a wake, polling picks up from
        // the task as the stream left it — settled or not.
        if let Followed::Woke(wake) = follow(client, &mut task, deadline, options, progress).await {
            return woken(client, task, wake, guard).await;
        }
    }

    loop {
        if is_settled(&task.status.state) {
            guard.disarm();
            return Ok(A2aExchange::Settled(task));
        }
        if Instant::now() >= deadline {
            return woken(client, task, Wake::Elapsed, guard).await;
        }

        let next_poll = (Instant::now() + limits.poll_interval).min(deadline);
        match wait(next_poll, options).await {
            Wake::Elapsed => {}
            wake => return woken(client, task, wake, guard).await,
        }

        let previous = task.status.state.clone();
        task = match client.get_task(&task.id, Some(0)).await {
            Ok(next) => next,
            Err(error) => {
                guard.disarm();
                return Err(A2aExchangeError::Poll {
                    task_id: task.id,
                    error,
                });
            }
        };
        if task.status.state != previous {
            progress(status_line(&task, None));
        }
    }
}

/// `remote task {id} is {state}`, and what the agent said, when it said
/// something.
fn status_line(task: &Task, said: Option<&str>) -> String {
    let mut line = format!(
        "remote task {} is {}",
        progress_id(&task.id),
        state_name(&task.status.state)
    );
    if let Some(said) = said {
        line.push_str(": ");
        line.push_str(said);
    }
    line
}

/// A task id as a progress line shows it.
///
/// The agent chose the id, so it is held to the rule its words are: one
/// line, no control characters, bounded.
fn progress_id(task_id: &str) -> String {
    remote_line(task_id, MAX_PROGRESS_ID_BYTES)
}

/// What a wait the run or the wait bound ended means for a task still
/// working: handed back, or — when the run was cancelled — cancelled.
async fn woken(
    client: &A2aClient,
    task: Task,
    wake: Wake,
    guard: CancelOnDrop,
) -> Result<A2aExchange, A2aExchangeError> {
    guard.disarm();
    let reason = match wake {
        Wake::Elapsed => A2aStillWorking::WaitBound(client.limits().max_wait),
        Wake::Stopping => A2aStillWorking::RunStopping,
        Wake::Cancelled => {
            let canceled = client.cancel_task(&task.id).await.map(drop);
            return Err(A2aExchangeError::Abandoned {
                task_id: task.id,
                canceled,
            });
        }
    };
    Ok(A2aExchange::StillWorking { task, reason })
}

/// Whether a task has stopped needing anyone to wait on it: terminal, or
/// paused for something only the caller can give.
pub fn is_settled(state: &TaskState) -> bool {
    state.is_terminal() || matches!(state, TaskState::InputRequired | TaskState::AuthRequired)
}

/// The lowercase name a model reads a task state by.
pub fn state_name(state: &TaskState) -> &'static str {
    match state {
        TaskState::Unspecified => "unspecified",
        TaskState::Submitted => "submitted",
        TaskState::Working => "working",
        TaskState::Completed => "completed",
        TaskState::Failed => "failed",
        TaskState::Canceled => "canceled",
        TaskState::InputRequired => "input-required",
        TaskState::Rejected => "rejected",
        TaskState::AuthRequired => "auth-required",
    }
}

/// Cancels a still-working task if the exchange is dropped while waiting on
/// it.
///
/// Every path that returns disarms it first, so it fires only when the future
/// itself is dropped. `Drop` cannot await, so the cancel is spawned detached:
/// it is skipped outside a Tokio context and may never run during runtime
/// teardown, which is why this is best-effort rather than a guarantee.
struct CancelOnDrop {
    pending: Option<(Arc<A2aClient>, String)>,
}

impl CancelOnDrop {
    fn new(client: &Arc<A2aClient>, task: &Task) -> Self {
        Self {
            pending: Some((Arc::clone(client), task.id.clone())),
        }
    }

    fn disarm(mut self) {
        self.pending = None;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let Some((client, task_id)) = self.pending.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            let _ = client.cancel_task(&task_id).await;
        });
    }
}
