use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
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
    session::{PermissionRuleAddress, PermissionRuleScope, RememberedRule, RuleStore},
    team::TeamStore,
};

use super::error::RuntimeError;

#[cfg(test)]
pub(crate) mod permission_contract;

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

/// Stable namespace context used to address remembered permission rules.
///
/// `session_id` identifies one resumable permission session. `project_id`, when
/// present, identifies the project namespace shared by several sessions. A
/// global rule ignores both identifiers and belongs to one store-wide global
/// namespace.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PermissionRuleContext {
    pub session_id: String,
    pub project_id: Option<String>,
}

impl PermissionRuleContext {
    pub(crate) fn validate_scope(&self, scope: PermissionRuleScope) -> Result<(), RuntimeError> {
        if scope == PermissionRuleScope::Project && self.project_id.is_none() {
            return Err(RuntimeError::OperationDenied(
                "project-scoped permission rules require a project_id".to_string(),
            ));
        }
        Ok(())
    }

    pub(crate) fn validate_persisted_scope(
        &self,
        scope: PermissionRuleScope,
    ) -> Result<(), RuntimeError> {
        self.validate_scope(scope)?;
        if scope == PermissionRuleScope::Process {
            return Err(RuntimeError::OperationDenied(
                "process-scoped permission rules belong to a live session and cannot be persisted"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

fn compare_duplicate_rules(left: &RememberedRule, right: &RememberedRule) -> std::cmp::Ordering {
    // `false < true`, so denial wins first. For equal verdicts, keep an
    // actionable reason over no reason, then use lexical reason/key order.
    left.allow
        .cmp(&right.allow)
        .then_with(|| match (&left.reason, &right.reason) {
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (left, right) => left.cmp(right),
        })
        .then_with(|| left.key.tool_name.cmp(&right.key.tool_name))
        .then_with(|| left.key.pattern.cmp(&right.key.pattern))
}

/// Collapses legacy duplicate rows into one fail-safe rule per exact address.
///
/// Denial wins conflicting verdicts. Equal verdicts prefer a reason, then
/// lexical reason/key order. The returned rules use [`RuleStore::rules`]'s
/// stable session/project/global and key ordering.
pub(crate) fn canonicalize_permission_rules(
    rules: impl IntoIterator<Item = RememberedRule>,
) -> Vec<RememberedRule> {
    let mut unique: HashMap<PermissionRuleAddress, RememberedRule> = HashMap::new();
    for rule in rules {
        let address = PermissionRuleAddress::from(&rule);
        match unique.entry(address) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(rule);
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if compare_duplicate_rules(&rule, entry.get()).is_lt() {
                    entry.insert(rule);
                }
            }
        }
    }

    let store = RuleStore::new();
    for rule in unique.into_values() {
        store.add_rule(rule);
    }
    store.rules()
}

/// Persistence backend for remembered permission rules.
///
/// This contract accepts only durable session, project, and global rules.
/// [`PermissionRuleScope::Process`] belongs to a live
/// [`SessionPermissionHandle`](crate::SessionPermissionHandle) and must be
/// rejected by mutation methods and omitted from loads.
///
/// Point operations are the authoritative mutation API. Implementations must
/// resolve the rule's namespace from [`PermissionRuleContext`] and perform each
/// upsert, revoke, or clear atomically within the backend's documented
/// concurrency boundary. Project-scoped mutation without a `project_id` is
/// invalid. Loading must return one deterministic, fail-safe rule per exact
/// address.
///
/// The older bulk methods remain for compatibility with callers, but mutating
/// defaults would permit partial writes after a later failure. Implementations
/// must therefore provide their own atomic `save_rules` and `clear_rules` as
/// well as the point contract. Only the read-only `load_rules` has a default.
pub trait PermissionRuleStore: Send + Sync {
    /// Atomically inserts or replaces one rule in its effective namespace.
    fn upsert_rule(
        &self,
        context: &PermissionRuleContext,
        rule: &RememberedRule,
    ) -> Result<(), RuntimeError>;

    /// Loads the unique durable rules applicable to `context` in stable order.
    fn load_applicable_rules(
        &self,
        context: &PermissionRuleContext,
    ) -> Result<Vec<RememberedRule>, RuntimeError>;

    /// Atomically revokes one exact address from its effective namespace.
    ///
    /// All legacy duplicate rows at that address are removed. Returns whether
    /// at least one row existed.
    fn revoke_rule(
        &self,
        context: &PermissionRuleContext,
        address: &PermissionRuleAddress,
    ) -> Result<bool, RuntimeError>;

    /// Atomically clears one effective scope and returns rows removed.
    ///
    /// The count includes legacy duplicates. Project scope requires a project
    /// id; global scope always names the one store-wide global namespace.
    fn clear_scope(
        &self,
        context: &PermissionRuleContext,
        scope: PermissionRuleScope,
    ) -> Result<usize, RuntimeError>;

    /// Persists the provided permission rules for a session, replacing any
    /// existing session-scoped rules. `project_id` is stored alongside each
    /// rule so that project-scoped rules can later be retrieved by other
    /// sessions that share the same project.
    ///
    /// Compatibility operation. Implementations must keep the whole mutation
    /// atomic within their documented concurrency boundary.
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
    ) -> Result<Vec<RememberedRule>, RuntimeError> {
        self.load_applicable_rules(&PermissionRuleContext {
            session_id: session_id.to_owned(),
            project_id: project_id.map(str::to_owned),
        })
    }

    /// Legacy creator-oriented clear operation.
    ///
    /// Implementations retain their released behavior of deleting rows written
    /// by `session_id`, regardless of effective scope, and must perform it
    /// atomically. New code should use [`Self::clear_scope`].
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
