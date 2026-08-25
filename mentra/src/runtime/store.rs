use std::{
    collections::HashSet,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    agent::{AgentConfig, AgentStatus, SpawnedAgentSummary, TeammateIdentity},
    background::BackgroundStore,
    memory::MemoryStore,
    memory::journal::AgentMemoryState,
    provider::ProviderId,
    runtime::TaskItem,
    session::permission::RememberedRule,
    team::TeamStore,
};
use std::path::Path;

use super::error::RuntimeError;

static NEXT_STORE_ID: AtomicU64 = AtomicU64::new(1);
#[cfg(test)]
static NEXT_TEST_STORE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedAgentRecord {
    pub(crate) id: String,
    pub(crate) runtime_identifier: String,
    pub(crate) name: String,
    pub(crate) model: String,
    pub(crate) provider_id: ProviderId,
    pub(crate) config: AgentConfig,
    pub(crate) hidden_tools: HashSet<String>,
    pub(crate) max_rounds: Option<usize>,
    pub(crate) teammate_identity: Option<TeammateIdentity>,
    pub(crate) rounds_since_task: usize,
    pub(crate) idle_requested: bool,
    pub(crate) status: AgentStatus,
    pub(crate) subagents: Vec<SpawnedAgentSummary>,
}

#[derive(Debug, Clone)]
pub struct LoadedAgentState {
    pub(crate) record: PersistedAgentRecord,
    pub(crate) memory: AgentMemoryState,
    /// When the store first wrote this agent, in seconds since the epoch.
    ///
    /// Storage metadata rather than agent state, which is why it lives here
    /// and not on [`PersistedAgentRecord`]: a store that keeps no history —
    /// the volatile one — has nothing to report and says so.
    pub(crate) created_at: Option<u64>,
    /// When the store last wrote this agent, in seconds since the epoch.
    pub(crate) updated_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskStateSnapshot {
    pub(crate) tasks: Vec<TaskItem>,
}

/// Persistence backend for agent records and working-memory snapshots.
///
/// Custom runtime backends implement this trait to store durable agent identity,
/// configuration, and transcript state.
pub trait AgentStore: Send + Sync {
    /// Returns whether runtime-managed auxiliary artifacts may be written to
    /// disk for agents backed by this store.
    ///
    /// Persistent stores allow artifacts by default. Volatile stores override
    /// this capability so features such as full tool-output spilling preserve
    /// their no-durable-trace contract.
    fn allows_disk_artifacts(&self) -> bool {
        true
    }

    fn prepare_recovery(&self) -> Result<(), RuntimeError>;
    fn create_agent(
        &self,
        record: &PersistedAgentRecord,
        memory: &AgentMemoryState,
    ) -> Result<(), RuntimeError>;
    fn save_agent_record(&self, record: &PersistedAgentRecord) -> Result<(), RuntimeError>;
    fn save_agent_memory(
        &self,
        agent_id: &str,
        memory: &AgentMemoryState,
    ) -> Result<(), RuntimeError>;
    fn load_agent(&self, agent_id: &str) -> Result<Option<LoadedAgentState>, RuntimeError>;
    /// Removes an agent's record and its persisted memory.
    ///
    /// Removing one without the other leaves a row that cannot be resumed, so
    /// implementations must remove both. Deleting an agent that is not there
    /// succeeds: the caller's goal is that it be gone.
    fn delete_agent(&self, agent_id: &str) -> Result<(), RuntimeError>;
    fn list_agents(&self) -> Result<Vec<LoadedAgentState>, RuntimeError>;
    fn list_agents_by_runtime(
        &self,
        runtime_identifier: &str,
    ) -> Result<Vec<LoadedAgentState>, RuntimeError>;
}

/// Persistence backend for tracked agent runs.
///
/// This trait stores lifecycle transitions for turns and interrupted runs.
pub trait RunStore: Send + Sync {
    fn start_run(&self, agent_id: &str) -> Result<String, RuntimeError>;
    fn update_run_state(
        &self,
        run_id: &str,
        state: &str,
        error: Option<&str>,
    ) -> Result<(), RuntimeError>;
    fn finish_run(&self, run_id: &str) -> Result<(), RuntimeError>;
    fn fail_run(&self, run_id: &str, error: &str) -> Result<(), RuntimeError>;
}

/// Persistence backend for the dependency-aware task board.
///
/// Task persistence is intentionally separate so applications can replace the
/// task board without reimplementing unrelated runtime storage.
pub trait TaskStore: Send + Sync {
    fn load_tasks(&self, namespace: &Path) -> Result<Vec<TaskItem>, RuntimeError>;
    fn capture_tasks(&self, namespace: &Path) -> Result<TaskStateSnapshot, RuntimeError>;
    fn restore_tasks(
        &self,
        namespace: &Path,
        snapshot: &TaskStateSnapshot,
    ) -> Result<(), RuntimeError>;
    fn replace_tasks(&self, namespace: &Path, tasks: &[TaskItem]) -> Result<(), RuntimeError>;

    /// Applies one read-modify-write operation to a namespace.
    ///
    /// The callback form keeps this method object-safe, so runtime code can use
    /// it through `dyn TaskStore`. The default preserves source compatibility
    /// for external stores by composing [`TaskStore::load_tasks`] and
    /// [`TaskStore::replace_tasks`], but that fallback cannot promise
    /// serialization across concurrent writers. Stores that can provide a
    /// transaction or lock should override this method.
    ///
    /// If `mutation` returns an error, the modified task vector must not be
    /// installed by overrides.
    fn mutate(
        &self,
        namespace: &Path,
        mutation: &mut dyn FnMut(&mut Vec<TaskItem>) -> Result<(), RuntimeError>,
    ) -> Result<(), RuntimeError> {
        let mut tasks = self.load_tasks(namespace)?;
        mutation(&mut tasks)?;
        self.replace_tasks(namespace, &tasks)
    }
}

/// Persistence backend for runtime audit hooks.
pub trait AuditStore: Send + Sync {
    fn record_audit_event(
        &self,
        scope: &str,
        event_type: &str,
        payload: serde_json::Value,
    ) -> Result<(), RuntimeError>;
}

/// Persistence backend for runtime leases.
///
/// Leases coordinate exclusive ownership when multiple runtime processes may try
/// to resume the same persisted agents.
pub trait LeaseStore: Send + Sync {
    fn acquire_lease(&self, key: &str, owner: &str, ttl: Duration) -> Result<bool, RuntimeError>;
    fn release_lease(&self, key: &str, owner: &str) -> Result<(), RuntimeError>;
}

/// Persistence backend for remembered permission rules.
///
/// Permission rules survive session restarts when backed by a persistent store.
///
/// The `project_id` parameter is an opaque string supplied by the consumer and
/// used to associate rules with a project for cross-session inheritance.
/// Mentra does not interpret its value.
pub trait PermissionRuleStore: Send + Sync {
    /// Persists the provided permission rules for a session, replacing any
    /// existing session-scoped rules. `project_id` is stored alongside each
    /// rule so that project-scoped rules can later be retrieved by other
    /// sessions that share the same project.
    fn save_rules(
        &self,
        session_id: &str,
        project_id: Option<&str>,
        rules: &[RememberedRule],
    ) -> Result<(), RuntimeError>;

    /// Loads all persisted permission rules that apply to the given session.
    ///
    /// The returned set is the union of:
    /// - Session-scoped rules where `session_id` matches.
    /// - Project-scoped rules where `project_id` matches (when provided).
    /// - Global-scoped rules (always included).
    fn load_rules(
        &self,
        session_id: &str,
        project_id: Option<&str>,
    ) -> Result<Vec<RememberedRule>, RuntimeError>;

    /// Removes all persisted permission rules for a session.
    fn clear_rules(&self, session_id: &str) -> Result<(), RuntimeError>;
}

/// Full persistence backend used by the runtime.
///
/// `RuntimeStore` is a composition trait over the narrower persistence seams
/// plus the collaboration and memory stores. Custom backends can implement the
/// smaller traits directly and then satisfy `RuntimeStore` automatically.
pub trait RuntimeStore:
    AgentStore
    + RunStore
    + TaskStore
    + AuditStore
    + LeaseStore
    + PermissionRuleStore
    + TeamStore
    + BackgroundStore
    + MemoryStore
    + Send
    + Sync
{
}

impl<T> RuntimeStore for T where
    T: AgentStore
        + RunStore
        + TaskStore
        + AuditStore
        + LeaseStore
        + PermissionRuleStore
        + TeamStore
        + BackgroundStore
        + MemoryStore
        + Send
        + Sync
{
}

pub(crate) fn next_id(prefix: &str) -> String {
    let counter = NEXT_STORE_ID.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{:x}-{:x}", now_nanos(), counter)
}

pub(crate) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub(crate) fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[cfg(not(test))]
pub(crate) fn default_store_dir() -> PathBuf {
    crate::default_paths::workspace_default_paths().root_dir
}

#[cfg(test)]
thread_local! {
    /// Every default store directory handed out on the current thread.
    ///
    /// Each one is unique, so recording them lets a test name the database its
    /// own builder would have used — which is the only way to assert that a
    /// builder given an explicit store left the default alone.
    static DEFAULT_STORE_DIRS: std::cell::RefCell<Vec<PathBuf>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
pub(crate) fn default_store_dir() -> PathBuf {
    let suffix = NEXT_TEST_STORE_ID.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir()
        .join("mentra-test-runtime")
        .join(format!("process-{}-{suffix}", std::process::id()));
    DEFAULT_STORE_DIRS.with(|dirs| dirs.borrow_mut().push(dir.clone()));
    dir
}

/// The paths every default store constructed on this thread would create:
/// the SQLite database with `store-sqlite` on, the file store's `agents`
/// directory with it off.
///
/// Only the first open or recovery creates anything at one, so an untouched
/// default store leaves nothing at these paths.
#[cfg(test)]
pub(crate) fn default_store_paths_on_this_thread() -> Vec<PathBuf> {
    #[cfg(feature = "store-sqlite")]
    let file_name = "runtime.sqlite";
    #[cfg(not(feature = "store-sqlite"))]
    let file_name = "agents";
    DEFAULT_STORE_DIRS.with(|dirs| {
        dirs.borrow()
            .iter()
            .map(|dir| dir.join(file_name))
            .collect()
    })
}
