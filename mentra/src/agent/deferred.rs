//! What a tool leaves for its agent to find at the next round.
//!
//! A tool that starts work and returns before the work is done — a delegated
//! run on another task, a request that will answer later — has no way to put
//! the result in front of the model once its own call has returned: the
//! [`ToolContext`](crate::tool::ToolContext) it was given borrows the agent
//! and is gone with the call. Background commands had one way around this
//! that was theirs alone: `background_run` writes a notification the runner
//! injects before the next model request. This is that path, generalized —
//! for anything a tool spawns, and for the two things a delegation has to
//! record on its parent besides a message.
//!
//! A [`Deferred`] handle is taken from the tool context while the call runs
//! and carried into whatever outlives it. Each [`DeferredItem`] it pushes is
//! applied by the agent's runner at the next round boundary, in the order
//! pushed, before the model is asked again:
//!
//! - a [`Context`](DeferredItem::Context) message is appended as canonical
//!   context, the way a background command's result is;
//! - a [`DelegationResult`](DeferredItem::DelegationResult) is appended as the
//!   `task` intrinsic appends one, with its artifact and edge, so a transcript
//!   reader following delegations finds it;
//! - a [`SubagentFinished`](DeferredItem::SubagentFinished) settles a child the
//!   parent registered with
//!   [`register_subagent`](crate::tool::ToolContext::register_subagent) and
//!   announces it on the parent's stream, exactly as
//!   [`finish_subagent`](crate::tool::ToolContext::finish_subagent) would have.
//!
//! An item that lands after the model's last message keeps the run going: a
//! run does not return while something it started has an answer the model has
//! not seen. That is the one behavioral difference from a background command,
//! whose notification waits for the next run on the agent.
//!
//! Items live in memory on the runtime, keyed by agent. They do not survive
//! the process, and need not: nothing that pushes them does either.
//!
//! For progress while background work runs, carry a
//! [`ToolProgress`](crate::tool::ToolProgress) from the tool context alongside
//! this handle. Progress reaches the parent's observers immediately and does
//! not add context or cause another model round.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use crate::{
    agent::{Agent, AgentEvent, SpawnedAgentStatus},
    error::RuntimeError,
    transcript::{DelegationArtifact, DelegationEdge},
};

/// One thing left for an agent to find at its next round.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum DeferredItem {
    /// A message for the model, appended as canonical context.
    Context(String),
    /// The result of a delegation the agent recorded the request for, as the
    /// `task` intrinsic records one.
    DelegationResult {
        content: String,
        delegation: DelegationArtifact,
        edge: Option<DelegationEdge>,
    },
    /// A registered subagent finished with `status`.
    SubagentFinished {
        id: String,
        status: SpawnedAgentStatus,
    },
}

/// Where every agent's deferred items wait: one queue per agent id, on the
/// runtime, shared by every handle to it.
#[derive(Clone, Default)]
pub(crate) struct DeferredQueue {
    by_agent: Arc<Mutex<HashMap<String, Vec<DeferredItem>>>>,
}

impl DeferredQueue {
    fn push(&self, agent_id: &str, item: DeferredItem) {
        self.lock()
            .entry(agent_id.to_string())
            .or_default()
            .push(item);
    }

    fn drain(&self, agent_id: &str) -> Vec<DeferredItem> {
        self.lock().remove(agent_id).unwrap_or_default()
    }

    fn has_pending(&self, agent_id: &str) -> bool {
        self.lock()
            .get(agent_id)
            .is_some_and(|items| !items.is_empty())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<DeferredItem>>> {
        self.by_agent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A tool's way to reach its agent after the call that made it has returned.
///
/// Taken with [`ToolContext::deferred`](crate::tool::ToolContext::deferred)
/// (or the parallel context's), and `Clone + Send + 'static` so it can be
/// moved into a spawned task and outlive the call. Everything pushed through
/// it reaches the agent at its next round boundary; see the module docs for
/// what each item does there.
#[derive(Clone)]
pub struct Deferred {
    queue: DeferredQueue,
    agent_id: String,
}

impl Deferred {
    pub(crate) fn new(queue: DeferredQueue, agent_id: String) -> Self {
        Self { queue, agent_id }
    }

    /// The agent the items go to.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Leaves `item` for the agent's next round.
    pub fn push(&self, item: DeferredItem) {
        self.queue.push(&self.agent_id, item);
    }
}

impl std::fmt::Debug for Deferred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deferred")
            .field("agent_id", &self.agent_id)
            .finish_non_exhaustive()
    }
}

impl Agent {
    /// Whether anything is waiting to be applied at this agent's next round.
    pub(super) fn has_deferred(&self) -> bool {
        self.runtime.collaboration.deferred.has_pending(&self.id)
    }

    /// Applies everything left for this agent since it last asked, in the
    /// order it was pushed. Called by the runner before each model request.
    pub(super) fn apply_deferred(&mut self) -> Result<(), RuntimeError> {
        let items = self.runtime.collaboration.deferred.drain(&self.id);
        if items.is_empty() {
            return Ok(());
        }
        for item in items {
            match item {
                DeferredItem::Context(content) => self.record_canonical_context(content)?,
                DeferredItem::DelegationResult {
                    content,
                    delegation,
                    edge,
                } => self.record_delegation_result(content, delegation, edge)?,
                DeferredItem::SubagentFinished { id, status } => {
                    if let Some(finished) = self.finish_subagent(&id, status) {
                        self.emit_event(AgentEvent::SubagentFinished { agent: finished });
                    }
                }
            }
        }
        self.sync_memory_snapshot();
        Ok(())
    }
}
