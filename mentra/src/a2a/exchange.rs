//! One request to a remote agent, waited on until it settles — as data.
//!
//! [`exchange`] is the loop behind [`A2aAgentTool`](crate::a2a::A2aAgentTool)
//! without the tool's words: it submits or resumes, polls the task, and hands
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
//! soon as the task exists, and the task is then polled with `GetTask` until
//! it [settles](is_settled), the client's wait bound
//! ([`A2aLimits::max_wait`](crate::a2a::A2aLimits::max_wait)) passes, or the
//! run ends. A cancelled run asks the remote agent to cancel the task before
//! the exchange returns; a run asked to *stop* hands the task back still
//! running. A future dropped mid-poll sends the cancel from a detached task,
//! best-effort, because `Drop` cannot await.

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use a2a_lf::{Message, Part, Role, SendMessageConfiguration, SendMessageResponse, Task, TaskState};
use tokio::time::Instant;

use super::client::A2aClient;
use super::error::A2aError;
use crate::runtime::RunOptions;

/// How often the wait between polls checks whether the run was cancelled.
///
/// A poll interval is seconds long, and the cancellation token is a flag
/// rather than a notification, so without this a cancelled run would wait out
/// the rest of the interval before noticing.
const INTERRUPT_CHECK_INTERVAL: Duration = Duration::from_millis(100);

/// What one exchange asks of the remote agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum A2aRequest {
    /// Send a message: a new task — in `context_id`'s conversation when
    /// given — or, with `task_id`, an answer to a task waiting for input.
    Send {
        text: String,
        task_id: Option<String>,
        context_id: Option<String>,
    },
    /// Wait on an existing task without sending anything.
    Resume { task_id: String },
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
/// cancellation, stop and deadline are checked between polls — and
/// `progress` hears each change of the task's state.
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
        A2aRequest::Send {
            text,
            task_id,
            context_id,
        } => {
            let mut message = Message::new(Role::User, vec![Part::text(text)]);
            message.task_id = task_id;
            message.context_id = context_id;
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

/// Polls a task [`submit`] returned until it settles, the client's wait bound
/// passes — counted from this call — or the run ends.
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

/// Polls a task until it settles, the wait bound passes, or the run ends.
async fn settle_from(
    client: &Arc<A2aClient>,
    mut task: Task,
    started: Instant,
    options: &RunOptions,
    progress: &(dyn Fn(String) + Send + Sync),
) -> Result<A2aExchange, A2aExchangeError> {
    let limits = client.limits();
    let guard = CancelOnDrop::new(client, &task);

    loop {
        if is_settled(&task.status.state) {
            guard.disarm();
            return Ok(A2aExchange::Settled(task));
        }

        let remaining = limits.max_wait.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            guard.disarm();
            let reason = A2aStillWorking::WaitBound(limits.max_wait);
            return Ok(A2aExchange::StillWorking { task, reason });
        }

        match wait(limits.poll_interval.min(remaining), options).await {
            Wake::Elapsed => {}
            Wake::Stopping => {
                guard.disarm();
                let reason = A2aStillWorking::RunStopping;
                return Ok(A2aExchange::StillWorking { task, reason });
            }
            Wake::Cancelled => {
                guard.disarm();
                let canceled = client.cancel_task(&task.id).await.map(drop);
                return Err(A2aExchangeError::Abandoned {
                    task_id: task.id,
                    canceled,
                });
            }
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
            progress(format!(
                "remote task {} is {}",
                task.id,
                state_name(&task.status.state)
            ));
        }
    }
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

/// Why the wait between polls ended.
enum Wake {
    Elapsed,
    Stopping,
    Cancelled,
}

/// Waits `duration`, waking early if the run is cancelled, passes its
/// deadline, or is asked to stop.
async fn wait(duration: Duration, options: &RunOptions) -> Wake {
    let deadline = Instant::now() + duration;
    loop {
        if options.check_limits().is_err() {
            return Wake::Cancelled;
        }
        if options.stop_requested() {
            return Wake::Stopping;
        }
        let now = Instant::now();
        if now >= deadline {
            return Wake::Elapsed;
        }
        tokio::time::sleep((deadline - now).min(INTERRUPT_CHECK_INTERVAL)).await;
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
