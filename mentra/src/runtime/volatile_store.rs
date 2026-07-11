//! An in-memory `RuntimeStore` for genuinely ephemeral runs.
//!
//! [`VolatileRuntimeStore`] satisfies the full `RuntimeStore` composition
//! (agent records, runs, tasks, audit events, leases, permission rules, team
//! state, background-task notifications, and long-term memory) entirely in
//! process memory. It never touches disk: no SQLite file is opened, no
//! transcript `.jsonl` snapshot is written, no directory is created.
//! Dropping every clone of the store leaves nothing behind.
//!
//! ## Isolation across runs
//!
//! The **recommended pattern** is to construct a fresh store per run —
//! [`VolatileRuntimeStore::new`] is trivial (no I/O, a handful of empty
//! collections behind one `Arc<Mutex<_>>`), so building one per ask is
//! cheap. A fresh store per run isolates runs by construction: there is
//! nothing to leak because nothing is shared.
//!
//! A host that instead **retains** one `VolatileRuntimeStore` across
//! multiple sequential runs (for example inside a pooled `Runtime`) does not
//! get that isolation for free. Several `RuntimeStore` methods have no
//! per-run scope in their signature: [`AgentStore::list_agents`] lists every
//! agent record the store has ever seen, and the `TeamStore`/`TaskStore`
//! seams are keyed by `team_dir`/`tasks_dir` paths and agent *names*, which
//! `AgentConfig::default()` gives the same value across every agent built in
//! one process. A retained store therefore behaves like a shared database:
//! two runs that use the same team directory, tasks directory, or agent name
//! will see each other's records, exactly as two `Agent::run`s pointed at
//! the same `SqliteRuntimeStore` path would.
//!
//! [`VolatileRuntimeStore::reset`] is the explicit isolation seam for that
//! case: it clears all in-memory state atomically, on every clone (clones
//! share the same backing state, like [`SqliteRuntimeStore`](super::SqliteRuntimeStore)'s clones share the
//! same file). A host that retains one store across runs must call
//! `reset()` between runs to get the same no-cross-run-visibility guarantee
//! that constructing a fresh store gives automatically.

mod background;
mod memory;
mod permission;
mod task;
mod team;

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use background::BackgroundState;
use memory::MemoryState;
use permission::PermissionState;
use task::TaskState;
use team::TeamState;

use crate::memory::journal::AgentMemoryState;

use super::{AgentStore, AuditStore, LeaseStore, LoadedAgentState, PersistedAgentRecord, RunStore};
use crate::runtime::RuntimeError;

/// Converts a filesystem path into the string key used to namespace
/// path-scoped state (`tasks_dir`, `team_dir`). Mirrors
/// [`SqliteRuntimeStore`](super::SqliteRuntimeStore)'s use of the path's
/// string form as a SQL key.
fn path_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Delivery/notification lifecycle shared by the team inbox and the
/// background-task notification queue: an entry starts `Pending`, moves to
/// `Inflight` while a round is actively reading it, and ends `Acked` (or is
/// requeued back to `Pending` when the run that read it fails).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryState {
    Pending,
    Inflight,
    Acked,
}

struct RunRecord {
    state: String,
    error: Option<String>,
}

struct LeaseEntry {
    owner: String,
    expires_at: Instant,
}

#[derive(Default)]
struct VolatileState {
    agents: HashMap<String, PersistedAgentRecord>,
    agent_memory: HashMap<String, AgentMemoryState>,
    agent_order: Vec<String>,
    runs: HashMap<String, RunRecord>,
    next_run_id: u64,
    leases: HashMap<String, LeaseEntry>,
    tasks: TaskState,
    team: TeamState,
    background: BackgroundState,
    permissions: PermissionState,
    memory: MemoryState,
}

/// An in-memory [`RuntimeStore`](super::RuntimeStore) that leaves no durable
/// trace. See the module docs for the isolation contract when one instance
/// is retained across multiple runs.
#[derive(Clone)]
pub struct VolatileRuntimeStore {
    state: Arc<Mutex<VolatileState>>,
}

impl VolatileRuntimeStore {
    /// Creates an empty volatile store. Construction is trivial (no I/O) —
    /// building a fresh instance per run is the recommended pattern; see the
    /// module docs for the retained-store alternative.
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(VolatileState::default())),
        }
    }

    /// Clears all in-memory state on every clone of this store.
    ///
    /// Call this between runs when retaining one `VolatileRuntimeStore`
    /// across multiple sequential runs (for example inside a pooled
    /// `Runtime`) to prevent one run's records from being visible to the
    /// next. See the module docs for why a retained store needs this.
    pub fn reset(&self) {
        *self.lock() = VolatileState::default();
    }

    fn lock(&self) -> MutexGuard<'_, VolatileState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for VolatileRuntimeStore {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentStore for VolatileRuntimeStore {
    fn prepare_recovery(&self) -> Result<(), RuntimeError> {
        // Nothing to recover: a volatile store never survives a process
        // restart, so there is no interrupted state to reconcile.
        Ok(())
    }

    fn create_agent(
        &self,
        record: &PersistedAgentRecord,
        memory: &AgentMemoryState,
    ) -> Result<(), RuntimeError> {
        let mut state = self.lock();
        if !state.agents.contains_key(&record.id) {
            state.agent_order.push(record.id.clone());
        }
        state.agents.insert(record.id.clone(), record.clone());
        state.agent_memory.insert(record.id.clone(), memory.clone());
        Ok(())
    }

    fn save_agent_record(&self, record: &PersistedAgentRecord) -> Result<(), RuntimeError> {
        let mut state = self.lock();
        if !state.agents.contains_key(&record.id) {
            state.agent_order.push(record.id.clone());
        }
        state.agents.insert(record.id.clone(), record.clone());
        Ok(())
    }

    fn save_agent_memory(
        &self,
        agent_id: &str,
        memory: &AgentMemoryState,
    ) -> Result<(), RuntimeError> {
        self.lock()
            .agent_memory
            .insert(agent_id.to_string(), memory.clone());
        Ok(())
    }

    fn load_agent(&self, agent_id: &str) -> Result<Option<LoadedAgentState>, RuntimeError> {
        let state = self.lock();
        let Some(record) = state.agents.get(agent_id).cloned() else {
            return Ok(None);
        };
        let Some(memory) = state.agent_memory.get(agent_id).cloned() else {
            return Err(RuntimeError::Store(format!(
                "Agent '{agent_id}' is missing persisted memory"
            )));
        };
        Ok(Some(LoadedAgentState { record, memory }))
    }

    fn list_agents(&self) -> Result<Vec<LoadedAgentState>, RuntimeError> {
        let state = self.lock();
        state
            .agent_order
            .iter()
            .map(|id| {
                let record = state
                    .agents
                    .get(id)
                    .cloned()
                    .ok_or_else(|| RuntimeError::Store(format!("Agent '{id}' disappeared")))?;
                let memory = state.agent_memory.get(id).cloned().ok_or_else(|| {
                    RuntimeError::Store(format!("Agent '{id}' is missing persisted memory"))
                })?;
                Ok(LoadedAgentState { record, memory })
            })
            .collect()
    }

    fn list_agents_by_runtime(
        &self,
        runtime_identifier: &str,
    ) -> Result<Vec<LoadedAgentState>, RuntimeError> {
        Ok(self
            .list_agents()?
            .into_iter()
            .filter(|loaded| loaded.record.runtime_identifier == runtime_identifier)
            .collect())
    }
}

impl RunStore for VolatileRuntimeStore {
    fn start_run(&self, _agent_id: &str) -> Result<String, RuntimeError> {
        let mut state = self.lock();
        state.next_run_id += 1;
        let run_id = format!("volatile-run-{}", state.next_run_id);
        state.runs.insert(
            run_id.clone(),
            RunRecord {
                state: "running".to_string(),
                error: None,
            },
        );
        Ok(run_id)
    }

    fn update_run_state(
        &self,
        run_id: &str,
        run_state: &str,
        error: Option<&str>,
    ) -> Result<(), RuntimeError> {
        // Matches the default store's UPDATE-affecting-zero-rows behavior:
        // updating an unknown run id is a silent no-op, not an error.
        let mut state = self.lock();
        if let Some(run) = state.runs.get_mut(run_id) {
            run.state = run_state.to_string();
            run.error = error.map(str::to_string);
        }
        Ok(())
    }

    fn finish_run(&self, run_id: &str) -> Result<(), RuntimeError> {
        self.update_run_state(run_id, "finished", None)
    }

    fn fail_run(&self, run_id: &str, error: &str) -> Result<(), RuntimeError> {
        self.update_run_state(run_id, "failed", Some(error))
    }
}

impl AuditStore for VolatileRuntimeStore {
    fn record_audit_event(
        &self,
        _scope: &str,
        _event_type: &str,
        _payload: serde_json::Value,
    ) -> Result<(), RuntimeError> {
        // `AuditStore` has no reader method — nothing in mentra ever reads
        // an audit event back. The volatile profile accepts the write and
        // discards it rather than growing an in-memory log nobody consumes.
        Ok(())
    }
}

impl LeaseStore for VolatileRuntimeStore {
    fn acquire_lease(&self, key: &str, owner: &str, ttl: Duration) -> Result<bool, RuntimeError> {
        let mut state = self.lock();
        let now = Instant::now();
        state.leases.retain(|_, lease| lease.expires_at > now);
        if state.leases.contains_key(key) {
            return Ok(false);
        }
        state.leases.insert(
            key.to_string(),
            LeaseEntry {
                owner: owner.to_string(),
                expires_at: now + ttl,
            },
        );
        Ok(true)
    }

    fn release_lease(&self, key: &str, owner: &str) -> Result<(), RuntimeError> {
        let mut state = self.lock();
        if state
            .leases
            .get(key)
            .is_some_and(|lease| lease.owner == owner)
        {
            state.leases.remove(key);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::{
        agent::{AgentConfig, AgentStatus},
        provider::ProviderId,
        runtime::{AgentStore, LeaseStore, RunStore, RuntimeError},
    };

    use super::{AgentMemoryState, PersistedAgentRecord, VolatileRuntimeStore};

    fn agent_record(id: &str) -> PersistedAgentRecord {
        PersistedAgentRecord {
            id: id.to_string(),
            runtime_identifier: "test-runtime".to_string(),
            name: format!("agent-{id}"),
            model: "test-model".to_string(),
            provider_id: ProviderId::new("test"),
            config: AgentConfig::default(),
            hidden_tools: Default::default(),
            max_rounds: None,
            teammate_identity: None,
            rounds_since_task: 0,
            idle_requested: false,
            status: AgentStatus::default(),
            subagents: Vec::new(),
        }
    }

    #[test]
    fn create_agent_then_load_round_trips() {
        let store = VolatileRuntimeStore::new();
        let record = agent_record("agent-1");
        let memory = AgentMemoryState::default();

        store.create_agent(&record, &memory).expect("create agent");

        let loaded = store
            .load_agent("agent-1")
            .expect("load agent")
            .expect("agent present");
        assert_eq!(loaded.record.id, "agent-1");
        assert_eq!(loaded.record.name, "agent-agent-1");
    }

    #[test]
    fn save_agent_record_upserts_without_prior_create() {
        let store = VolatileRuntimeStore::new();
        let mut record = agent_record("agent-2");
        store.save_agent_record(&record).expect("save record");

        let err = store
            .load_agent("agent-2")
            .expect_err("memory should be missing until it is saved");
        assert!(matches!(err, RuntimeError::Store(_)));

        record.name = "renamed".to_string();
        store
            .save_agent_record(&record)
            .expect("save updated record");
        store
            .save_agent_memory("agent-2", &AgentMemoryState::default())
            .expect("save memory");

        let loaded = store
            .load_agent("agent-2")
            .expect("load agent")
            .expect("agent present");
        assert_eq!(loaded.record.name, "renamed");
    }

    #[test]
    fn list_agents_reflects_creation_order() {
        let store = VolatileRuntimeStore::new();
        let memory = AgentMemoryState::default();
        store
            .create_agent(&agent_record("first"), &memory)
            .expect("create first");
        store
            .create_agent(&agent_record("second"), &memory)
            .expect("create second");

        let ids: Vec<_> = store
            .list_agents()
            .expect("list agents")
            .into_iter()
            .map(|loaded| loaded.record.id)
            .collect();
        assert_eq!(ids, vec!["first".to_string(), "second".to_string()]);
    }

    #[test]
    fn lease_round_trips_and_frees_on_release() {
        let store = VolatileRuntimeStore::new();
        assert!(
            store
                .acquire_lease("agent:x", "owner-1", Duration::from_secs(60))
                .expect("acquire")
        );
        assert!(
            !store
                .acquire_lease("agent:x", "owner-2", Duration::from_secs(60))
                .expect("second acquire"),
            "lease should still be held by owner-1"
        );

        store.release_lease("agent:x", "owner-1").expect("release");
        assert!(
            store
                .acquire_lease("agent:x", "owner-2", Duration::from_secs(60))
                .expect("reacquire after release")
        );
    }

    #[test]
    fn reset_clears_all_state() {
        let store = VolatileRuntimeStore::new();
        let memory = AgentMemoryState::default();
        store
            .create_agent(&agent_record("agent-1"), &memory)
            .expect("create agent");
        store
            .acquire_lease("agent:agent-1", "owner", Duration::from_secs(60))
            .expect("acquire lease");
        let run_id = store.start_run("agent-1").expect("start run");

        store.reset();

        assert!(store.list_agents().expect("list agents").is_empty());
        assert!(
            store
                .acquire_lease("agent:agent-1", "owner-2", Duration::from_secs(60))
                .expect("lease is free after reset")
        );
        // The run id from before reset() no longer resolves to anything;
        // updating it is a silent no-op, matching the default store.
        store
            .update_run_state(&run_id, "finished", None)
            .expect("update on a stale run id is a no-op");
    }

    #[test]
    fn cloned_store_shares_state() {
        let store = VolatileRuntimeStore::new();
        let clone = store.clone();
        let memory = AgentMemoryState::default();

        clone
            .create_agent(&agent_record("shared"), &memory)
            .expect("create via clone");

        assert!(
            store
                .load_agent("shared")
                .expect("load via original")
                .is_some()
        );
    }
}
