use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard},
};

use crate::{ContentBlock, Message, error::RuntimeError, runtime::RunOptions};

use super::Agent;

/// Controls how many queued entries are injected at one eligible boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueueMode {
    /// Drain every currently queued entry into one model round.
    All,
    /// Drain exactly one entry per eligible boundary.
    #[default]
    OneAtATime,
}

#[derive(Default)]
struct SteeringQueues {
    steer: VecDeque<Vec<ContentBlock>>,
    follow_up: VecDeque<Vec<ContentBlock>>,
    steer_mode: QueueMode,
    follow_up_mode: QueueMode,
}

/// Cloneable, agent-scoped handle for live steering and deferred follow-ups.
///
/// Obtain this handle before calling [`Agent::run`](crate::Agent::run). `steer`
/// entries are eligible at either committed round boundary; `follow_up`
/// entries are eligible only when a tool-free assistant response would
/// otherwise stop the run. Queues are in-memory and survive sequential runs of
/// this agent, but are never shared with another agent on the same runtime.
#[derive(Clone, Default)]
pub struct SteeringHandle {
    queues: Arc<Mutex<SteeringQueues>>,
}

impl SteeringHandle {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Enqueues context for the next eligible round boundary.
    pub fn steer(&self, content: impl Into<Vec<ContentBlock>>) {
        let content = content.into();
        if !content.is_empty() {
            self.lock().steer.push_back(content);
        }
    }

    /// Enqueues context used only when a run would otherwise stop.
    pub fn follow_up(&self, content: impl Into<Vec<ContentBlock>>) {
        let content = content.into();
        if !content.is_empty() {
            self.lock().follow_up.push_back(content);
        }
    }

    /// Removes steering entries that have not yet been injected.
    pub fn clear_steer(&self) {
        self.lock().steer.clear();
    }

    /// Removes follow-up entries that have not yet been injected.
    pub fn clear_follow_up(&self) {
        self.lock().follow_up.clear();
    }

    /// Returns whether either queue contains an entry awaiting injection.
    pub fn has_pending(&self) -> bool {
        let queues = self.lock();
        !queues.steer.is_empty() || !queues.follow_up.is_empty()
    }

    /// Sets the steering drain mode for subsequent boundaries.
    pub fn set_steer_mode(&self, mode: QueueMode) {
        self.lock().steer_mode = mode;
    }

    /// Sets the follow-up drain mode for subsequent would-stop boundaries.
    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        self.lock().follow_up_mode = mode;
    }

    pub(crate) fn has_steer(&self) -> bool {
        !self.lock().steer.is_empty()
    }

    pub(crate) fn has_follow_up(&self) -> bool {
        !self.lock().follow_up.is_empty()
    }

    fn drain_steer(&self) -> Vec<Vec<ContentBlock>> {
        let mut queues = self.lock();
        let mode = queues.steer_mode;
        drain(&mut queues.steer, mode)
    }

    fn drain_follow_up(&self) -> Vec<Vec<ContentBlock>> {
        let mut queues = self.lock();
        let mode = queues.follow_up_mode;
        drain(&mut queues.follow_up, mode)
    }

    fn prepend_steer(&self, entries: Vec<Vec<ContentBlock>>) {
        prepend(&mut self.lock().steer, entries);
    }

    fn prepend_follow_up(&self, entries: Vec<Vec<ContentBlock>>) {
        prepend(&mut self.lock().follow_up, entries);
    }

    fn lock(&self) -> MutexGuard<'_, SteeringQueues> {
        self.queues
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Agent {
    /// Returns an agent-scoped handle suitable for use while `run(&mut self)`
    /// holds the mutable agent borrow.
    pub fn steering_handle(&self) -> SteeringHandle {
        self.steering.clone()
    }

    /// Idle convenience for enqueueing a steer on this agent.
    pub fn steer(&self, content: impl Into<Vec<ContentBlock>>) {
        self.steering.steer(content);
    }

    /// Idle convenience for enqueueing a would-stop follow-up on this agent.
    pub fn follow_up(&self, content: impl Into<Vec<ContentBlock>>) {
        self.steering.follow_up(content);
    }

    /// Starts an idle run from the next queued steer.
    ///
    /// This is the only automatic consumption point for a steer while no run is
    /// active. Follow-ups remain reserved for a running turn's would-stop
    /// boundary. A failed run prepends the consumed entry back onto the queue.
    pub async fn run_queued(&mut self, options: RunOptions) -> Result<Message, RuntimeError> {
        let Some(content) = self.drain_steer() else {
            return Err(RuntimeError::OperationDenied(
                "no queued steering input is available".to_string(),
            ));
        };

        let result = self.run(content, options).await;
        if result.is_err() {
            // `run` normally requeues on its rollback path. This second call is
            // intentionally idempotent and covers errors raised before the run
            // checkpoint is established.
            self.requeue_inflight_steering();
        }
        result
    }

    pub(super) fn has_pending_steer(&self) -> bool {
        self.steering.has_steer()
    }

    pub(super) fn has_pending_follow_up(&self) -> bool {
        self.steering.has_follow_up()
    }

    pub(super) fn drain_steer(&mut self) -> Option<Vec<ContentBlock>> {
        let entries = self.steering.drain_steer();
        if entries.is_empty() {
            return None;
        }
        let content = flatten(&entries);
        self.inflight_steer.extend(entries);
        Some(content)
    }

    pub(super) fn drain_follow_up(&mut self) -> Option<Vec<ContentBlock>> {
        let entries = self.steering.drain_follow_up();
        if entries.is_empty() {
            return None;
        }
        let content = flatten(&entries);
        self.inflight_follow_up.extend(entries);
        Some(content)
    }

    pub(super) fn clear_inflight_steering(&mut self) {
        self.inflight_steer.clear();
        self.inflight_follow_up.clear();
    }

    pub(super) fn requeue_inflight_steering(&mut self) {
        self.steering
            .prepend_steer(std::mem::take(&mut self.inflight_steer));
        self.steering
            .prepend_follow_up(std::mem::take(&mut self.inflight_follow_up));
    }
}

fn drain(queue: &mut VecDeque<Vec<ContentBlock>>, mode: QueueMode) -> Vec<Vec<ContentBlock>> {
    match mode {
        QueueMode::All => queue.drain(..).collect(),
        QueueMode::OneAtATime => queue.pop_front().into_iter().collect(),
    }
}

fn prepend(queue: &mut VecDeque<Vec<ContentBlock>>, entries: Vec<Vec<ContentBlock>>) {
    for entry in entries.into_iter().rev() {
        queue.push_front(entry);
    }
}

fn flatten(entries: &[Vec<ContentBlock>]) -> Vec<ContentBlock> {
    entries.iter().flatten().cloned().collect()
}
