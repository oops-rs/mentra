//! Settling a task by following its event stream.
//!
//! A subscription hears each change as it happens rather than at the next
//! poll, and hears the status messages a poll would miss between two reads.
//! It is an optimisation over polling and never a replacement for it: the
//! stream is only ever *read*, so whatever goes wrong with it — refused,
//! dropped, idle, malformed, or larger than a reply may be — leaves the task
//! as it last stood and the caller polls from there. Nothing on this path
//! sends the message again.

#[cfg(test)]
mod tests;

use a2a_lf::{StreamResponse, Task, TaskArtifactUpdateEvent, TaskStatus};
use futures_util::StreamExt;
use tokio::time::Instant;

use super::wait::{Wake, race};
use super::{is_settled, state_name};
use crate::a2a::client::A2aClient;
use crate::a2a::output::remote_line;
use crate::runtime::RunOptions;

/// Bound on the remote agent's words in one progress line.
const MAX_PROGRESS_TEXT_BYTES: usize = 512;

/// How following a task's stream ended.
pub(super) enum Followed {
    /// The task settled, the stream ended, or it failed: `task` is as the
    /// stream last left it, and polling decides what happens next.
    Ended,
    /// The run or the wait bound woke the wait first.
    Woke(Wake),
}

/// Applies a task's events to `task` until it settles, the stream ends, or
/// `deadline` or the run wakes the wait.
///
/// The task held is bounded as a `GetTask` reply is: once the artifacts the
/// stream has added outgrow the client's reply bound, the stream is left and
/// polling reads the task — and refuses it — whole.
pub(super) async fn follow(
    client: &A2aClient,
    task: &mut Task,
    deadline: Instant,
    options: &RunOptions,
    progress: &(dyn Fn(String) + Send + Sync),
) -> Followed {
    let mut events = match race(client.subscribe_to_task(&task.id), deadline, options).await {
        Err(wake) => return Followed::Woke(wake),
        Ok(Err(_)) => return Followed::Ended,
        Ok(Ok(events)) => events,
    };
    let mut budget = client.limits().max_response_bytes;

    loop {
        let event = match race(events.next(), deadline, options).await {
            Err(wake) => return Followed::Woke(wake),
            Ok(Some(Ok(event))) => event,
            Ok(Some(Err(_)) | None) => return Followed::Ended,
        };
        match event {
            StreamResponse::Task(snapshot) => {
                let changed = snapshot.status.state != task.status.state;
                *task = snapshot;
                if changed {
                    progress(status_line(task, None));
                }
            }
            StreamResponse::StatusUpdate(update) => apply_status(task, update.status, progress),
            StreamResponse::ArtifactUpdate(update) => {
                let size = serde_json::to_vec(&update.artifact).map_or(usize::MAX, |v| v.len());
                let Some(left) = budget.checked_sub(size) else {
                    return Followed::Ended;
                };
                budget = left;
                apply_artifact(task, update);
            }
            // A message has no place in a task's state.
            StreamResponse::Message(_) => {}
        }
        if is_settled(&task.status.state) {
            return Followed::Ended;
        }
    }
}

/// Takes a status update, reporting a change of state or a status message.
fn apply_status(task: &mut Task, status: TaskStatus, progress: &(dyn Fn(String) + Send + Sync)) {
    let changed = status.state != task.status.state;
    let said = status
        .message
        .as_ref()
        .map(|message| {
            message
                .parts
                .iter()
                .filter_map(|part| part.as_text())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .map(|text| remote_line(&text, MAX_PROGRESS_TEXT_BYTES))
        .filter(|text| !text.is_empty());
    task.status = status;
    if changed || said.is_some() {
        progress(status_line(task, said.as_deref()));
    }
}

/// `remote task {id} is {state}`, and what the agent said, when it said
/// something.
fn status_line(task: &Task, said: Option<&str>) -> String {
    let mut line = format!(
        "remote task {} is {}",
        task.id,
        state_name(&task.status.state)
    );
    if let Some(said) = said {
        line.push_str(": ");
        line.push_str(said);
    }
    line
}

/// Takes an artifact update: a new artifact, a replacement, or — when it
/// says `append` — more parts for one already held.
fn apply_artifact(task: &mut Task, update: TaskArtifactUpdateEvent) {
    let artifacts = task.artifacts.get_or_insert_with(Vec::new);
    let held = artifacts
        .iter_mut()
        .find(|artifact| artifact.artifact_id == update.artifact.artifact_id);
    match held {
        Some(held) if update.append == Some(true) => held.parts.extend(update.artifact.parts),
        Some(held) => *held = update.artifact,
        None => artifacts.push(update.artifact),
    }
}
