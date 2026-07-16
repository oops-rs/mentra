use std::{future::Future, path::PathBuf, pin::Pin};

use tokio::sync::watch;

use crate::{error::RuntimeError, runtime::RuntimeHandle, team::TeamMessage};

use super::{Agent, AgentSnapshot, AgentStatus};

/// Owned future returned by [`Agent`] and [`AgentWaitHandle`] wait helpers.
///
/// The future does not borrow the agent, so it can be polled concurrently with
/// a call that holds `&mut Agent`, including [`Agent::run`](crate::Agent::run).
pub type AgentWaitFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Cloneable observation handle for an agent's snapshot and teammate inbox.
#[derive(Clone)]
pub struct AgentWaitHandle {
    snapshots: watch::Receiver<AgentSnapshot>,
    runtime: RuntimeHandle,
    team_dir: PathBuf,
    agent_name: String,
}

impl AgentWaitHandle {
    /// Resolves with the first current or future snapshot satisfying `predicate`.
    /// If the agent is dropped first, the final published snapshot is returned.
    pub fn wait_for_snapshot<P>(&self, predicate: P) -> AgentWaitFuture<AgentSnapshot>
    where
        P: Fn(&AgentSnapshot) -> bool + Send + 'static,
    {
        let mut snapshots = self.snapshots.clone();
        Box::pin(async move {
            loop {
                let snapshot = snapshots.borrow().clone();
                if predicate(&snapshot) {
                    return snapshot;
                }
                if snapshots.changed().await.is_err() {
                    return snapshots.borrow().clone();
                }
            }
        })
    }

    /// Waits for the relevant run generation to become terminal.
    ///
    /// If called while a run is active, this waits for that generation. If
    /// called while the agent is initially idle or already terminal, it waits
    /// for the *next* generation, avoiding an immediate stale return from a
    /// previous run. Terminal statuses are `Finished`, `Failed`, and
    /// `Interrupted`; the initial `Idle` snapshot is not a completed run.
    pub fn wait_until_idle(&self) -> AgentWaitFuture<AgentSnapshot> {
        let snapshot = self.snapshots.borrow().clone();
        let target_generation = if is_active(&snapshot.status) {
            snapshot.run_generation
        } else {
            snapshot.run_generation.saturating_add(1)
        };
        self.wait_for_snapshot(move |snapshot| {
            snapshot.run_generation >= target_generation && is_terminal(&snapshot.status)
        })
    }

    /// Waits for and consumes the next batch of teammate replies.
    ///
    /// This is a host-consumption API, not a non-destructive observer. The
    /// underlying inbox read moves pending rows to the store's inflight state
    /// and resets `pending_team_messages`; the returned messages will therefore
    /// not also be injected into a later provider request. Do not race this
    /// helper with `Agent::run` reading the same inbox. The next successful run
    /// acknowledges inflight rows; a failed run requeues them.
    pub fn wait_for_teammate_reply(
        &self,
    ) -> AgentWaitFuture<Result<Vec<TeamMessage>, RuntimeError>> {
        let snapshots = self.clone();
        let runtime = self.runtime.clone();
        let team_dir = self.team_dir.clone();
        let agent_name = self.agent_name.clone();
        Box::pin(async move {
            snapshots
                .wait_for_snapshot(|snapshot| snapshot.pending_team_messages > 0)
                .await;
            runtime.read_team_inbox(&team_dir, &agent_name)
        })
    }
}

impl Agent {
    /// Returns a cloneable observation handle that does not borrow this agent.
    pub fn wait_handle(&self) -> AgentWaitHandle {
        AgentWaitHandle {
            snapshots: self.watch_snapshot(),
            runtime: self.runtime.clone(),
            team_dir: self.config.team.team_dir.clone(),
            agent_name: self.name.clone(),
        }
    }

    /// Owned-future convenience for [`AgentWaitHandle::wait_for_snapshot`].
    pub fn wait_for_snapshot<P>(&self, predicate: P) -> AgentWaitFuture<AgentSnapshot>
    where
        P: Fn(&AgentSnapshot) -> bool + Send + 'static,
    {
        self.wait_handle().wait_for_snapshot(predicate)
    }

    /// Owned-future convenience for [`AgentWaitHandle::wait_until_idle`].
    pub fn wait_until_idle(&self) -> AgentWaitFuture<AgentSnapshot> {
        self.wait_handle().wait_until_idle()
    }

    /// Owned-future convenience for [`AgentWaitHandle::wait_for_teammate_reply`].
    pub fn wait_for_teammate_reply(
        &self,
    ) -> AgentWaitFuture<Result<Vec<TeamMessage>, RuntimeError>> {
        self.wait_handle().wait_for_teammate_reply()
    }
}

fn is_active(status: &AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::AwaitingModel | AgentStatus::Streaming | AgentStatus::ExecutingTool { .. }
    )
}

fn is_terminal(status: &AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Finished | AgentStatus::Failed(_) | AgentStatus::Interrupted
    )
}
