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

use std::collections::HashMap;

use a2a_lf::{Artifact, StreamResponse, Task, TaskArtifactUpdateEvent, TaskStatus};
use futures_util::StreamExt;
use tokio::time::Instant;

use super::wait::{Wake, race};
use super::{is_settled, progress_id, status_line};
use crate::a2a::client::A2aClient;
use crate::a2a::output::remote_line;
use crate::runtime::RunOptions;

/// Bound on the remote agent's words in one progress line.
const MAX_PROGRESS_TEXT_BYTES: usize = 512;

/// How following a task's stream ended.
pub(super) enum Followed {
    /// The task settled, or the stream was given up: `task` is as the stream
    /// last left it, and polling decides what happens next.
    Ended,
    /// The run or the wait bound woke the wait first.
    Woke(Wake),
}

/// Why a stream was given up before the task settled.
enum GaveUp {
    Woke(Wake),
    /// Said in a progress line, so a host can see why it is polling.
    Because(String),
}

/// Applies a task's events to `task` until it settles, the stream is given
/// up, or `deadline` or the run wakes the wait.
///
/// A stream given up for any reason but a wake is reported in one progress
/// line saying why, then left to polling.
pub(super) async fn follow(
    client: &A2aClient,
    task: &mut Task,
    deadline: Instant,
    options: &RunOptions,
    progress: &(dyn Fn(String) + Send + Sync),
) -> Followed {
    match stream_until_settled(client, task, deadline, options, progress).await {
        Ok(()) => Followed::Ended,
        Err(GaveUp::Woke(wake)) => Followed::Woke(wake),
        Err(GaveUp::Because(why)) => {
            progress(format!(
                "stopped following remote task {} ({why}); polling it instead",
                progress_id(&task.id)
            ));
            Followed::Ended
        }
    }
}

async fn stream_until_settled(
    client: &A2aClient,
    task: &mut Task,
    deadline: Instant,
    options: &RunOptions,
    progress: &(dyn Fn(String) + Send + Sync),
) -> Result<(), GaveUp> {
    let mut events = race(client.subscribe_to_task(&task.id), deadline, options)
        .await
        .map_err(GaveUp::Woke)?
        .map_err(|error| GaveUp::Because(format!("subscribing failed: {error}")))?;
    let mut held = Held::new(client.limits().max_response_bytes);
    held.reset(task);

    loop {
        let event = match race(events.next(), deadline, options).await {
            Err(wake) => return Err(GaveUp::Woke(wake)),
            Ok(Some(Ok(event))) => event,
            Ok(Some(Err(error))) => {
                return Err(GaveUp::Because(format!("the stream failed: {error}")));
            }
            Ok(None) => {
                let why = "the stream ended before the task settled";
                return Err(GaveUp::Because(why.to_string()));
            }
        };
        match event {
            StreamResponse::Task(snapshot) => {
                let changed = snapshot.status.state != task.status.state;
                *task = snapshot;
                held.reset(task);
                if changed {
                    progress(status_line(task, None));
                }
            }
            StreamResponse::StatusUpdate(update) => apply_status(task, update.status, progress),
            StreamResponse::ArtifactUpdate(update) => {
                held.update(&update);
                apply_artifact(task, update);
            }
            // A message has no place in a task's state.
            StreamResponse::Message(_) => {}
        }
        if held.total > held.limit {
            return Err(GaveUp::Because(format!(
                "its artifacts outgrew the {} byte reply bound",
                held.limit
            )));
        }
        if is_settled(&task.status.state) {
            return Ok(());
        }
    }
}

/// The bytes of artifacts the followed task holds, by artifact.
///
/// The task is bounded as a `GetTask` reply is: once what it holds outgrows
/// the client's reply bound, the stream is given up and polling reads the
/// task — and refuses it — whole. A replacement counts as its own size, not
/// as growth, and a snapshot counts as what it holds.
struct Held {
    sizes: HashMap<String, usize>,
    total: usize,
    limit: usize,
}

impl Held {
    fn new(limit: usize) -> Self {
        Self {
            sizes: HashMap::new(),
            total: 0,
            limit,
        }
    }

    /// Counts what a task snapshot holds, replacing what was counted.
    fn reset(&mut self, task: &Task) {
        self.sizes.clear();
        self.total = 0;
        for artifact in task.artifacts.as_deref().unwrap_or_default() {
            self.set(&artifact.artifact_id, size_of(artifact));
        }
    }

    /// Counts an update as [`apply_artifact`] applies it.
    fn update(&mut self, update: &TaskArtifactUpdateEvent) {
        let id = &update.artifact.artifact_id;
        let size = size_of(&update.artifact);
        let held = self.sizes.get(id).copied();
        let size = match held {
            Some(held) if update.append == Some(true) => held.saturating_add(size),
            _ => size,
        };
        self.set(id, size);
    }

    fn set(&mut self, id: &str, size: usize) {
        let previous = self.sizes.insert(id.to_string(), size).unwrap_or(0);
        self.total = self.total.saturating_sub(previous).saturating_add(size);
    }
}

/// An artifact's size as JSON, which is how a reply would carry it.
fn size_of(artifact: &Artifact) -> usize {
    serde_json::to_vec(artifact).map_or(usize::MAX, |json| json.len())
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
