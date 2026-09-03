use std::{
    fmt,
    path::PathBuf,
    sync::{
        Arc, RwLock, Weak,
        atomic::{AtomicU64, Ordering as AtomicOrdering},
    },
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    provider::{ProviderError, TokenUsage},
    runtime::{AuditStore, RuntimeStore, error::RuntimeError},
    tool::{ToolAudience, ToolAuthorizationOutcome, ToolAuthorizationPreview, ToolResultContent},
};

static NEXT_EXECUTION_HOOK_REGISTRATION_ID: AtomicU64 = AtomicU64::new(1);

fn next_execution_hook_registration_id() -> u64 {
    NEXT_EXECUTION_HOOK_REGISTRATION_ID
        .fetch_update(AtomicOrdering::Relaxed, AtomicOrdering::Relaxed, |id| {
            id.checked_add(1)
        })
        .expect("execution hook registration identifiers exhausted")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuntimeHookEvent {
    AuthorizationDenied {
        agent_id: String,
        action: String,
        detail: String,
    },
    ToolAuthorizationStarted {
        agent_id: String,
        tool_name: String,
        tool_call_id: String,
        preview: ToolAuthorizationPreview,
    },
    ToolAuthorizationFinished {
        agent_id: String,
        tool_name: String,
        tool_call_id: String,
        outcome: ToolAuthorizationOutcome,
        reason: Option<String>,
    },
    ToolAuthorizationBlocked {
        agent_id: String,
        tool_name: String,
        tool_call_id: String,
        outcome: ToolAuthorizationOutcome,
        reason: Option<String>,
    },
    RecoveryPrepared {
        runtime_instance_id: String,
    },
    ModelRequestStarted {
        agent_id: String,
        model: String,
        attempt: usize,
    },
    ModelRequestFinished {
        agent_id: String,
        model: String,
        attempt: usize,
        success: bool,
        error: Option<String>,
    },
    ModelResponseFinished {
        agent_id: String,
        model: String,
        attempt: usize,
        success: bool,
        error: Option<String>,
        stop_reason: Option<String>,
        usage: Option<TokenUsage>,
    },
    ToolExecutionStarted {
        agent_id: String,
        tool_name: String,
        tool_call_id: String,
    },
    ToolExecutionFinished {
        agent_id: String,
        tool_name: String,
        tool_call_id: String,
        is_error: bool,
        error: Option<String>,
        output_preview: String,
        /// Opaque host metadata attached via `ToolOutput::details`, carried up
        /// to this observability boundary. Never sent to a provider.
        #[serde(default)]
        details: Option<serde_json::Value>,
    },
    PolicyDenied {
        agent_id: String,
        tool_name: String,
        reason: String,
    },
    BackgroundTaskStarted {
        agent_id: String,
        task_id: String,
        command: String,
        cwd: PathBuf,
    },
    BackgroundTaskFinished {
        agent_id: String,
        task_id: String,
        status: String,
    },
    MemorySearchStarted {
        agent_id: String,
        limit: usize,
        query_preview: String,
    },
    MemorySearchFinished {
        agent_id: String,
        success: bool,
        result_count: usize,
        error: Option<String>,
    },
    MemoryIngestStarted {
        agent_id: String,
        source_revision: u64,
    },
    MemoryIngestFinished {
        agent_id: String,
        source_revision: u64,
        success: bool,
        stored_records: usize,
        error: Option<String>,
    },
    MemoryCompactionProposed {
        agent_id: String,
        base_revision: u64,
        transcript_path: PathBuf,
    },
    MemoryCompactionApplied {
        agent_id: String,
        base_revision: u64,
        resulting_history_len: usize,
    },
    MemoryCompactionSkipped {
        agent_id: String,
        base_revision: u64,
    },
    RunAborted {
        agent_id: String,
        reason: String,
    },
    ToolExecutionBlocked {
        agent_id: String,
        tool_name: String,
        tool_call_id: String,
        reason: String,
    },
}

impl RuntimeHookEvent {
    fn scope(&self) -> String {
        match self {
            Self::AuthorizationDenied { agent_id, .. } => agent_id.clone(),
            Self::ToolAuthorizationStarted { agent_id, .. } => agent_id.clone(),
            Self::ToolAuthorizationFinished { agent_id, .. } => agent_id.clone(),
            Self::ToolAuthorizationBlocked { agent_id, .. } => agent_id.clone(),
            Self::RecoveryPrepared {
                runtime_instance_id,
            } => runtime_instance_id.clone(),
            Self::ModelRequestStarted { agent_id, .. }
            | Self::ModelRequestFinished { agent_id, .. }
            | Self::ModelResponseFinished { agent_id, .. }
            | Self::ToolExecutionStarted { agent_id, .. }
            | Self::ToolExecutionFinished { agent_id, .. }
            | Self::PolicyDenied { agent_id, .. }
            | Self::BackgroundTaskStarted { agent_id, .. }
            | Self::BackgroundTaskFinished { agent_id, .. }
            | Self::MemorySearchStarted { agent_id, .. }
            | Self::MemorySearchFinished { agent_id, .. }
            | Self::MemoryIngestStarted { agent_id, .. }
            | Self::MemoryIngestFinished { agent_id, .. }
            | Self::MemoryCompactionProposed { agent_id, .. }
            | Self::MemoryCompactionApplied { agent_id, .. }
            | Self::MemoryCompactionSkipped { agent_id, .. }
            | Self::RunAborted { agent_id, .. }
            | Self::ToolExecutionBlocked { agent_id, .. } => agent_id.clone(),
        }
    }

    fn event_type(&self) -> &'static str {
        match self {
            Self::AuthorizationDenied { .. } => "authorization_denied",
            Self::ToolAuthorizationStarted { .. } => "tool_authorization_started",
            Self::ToolAuthorizationFinished { .. } => "tool_authorization_finished",
            Self::ToolAuthorizationBlocked { .. } => "tool_authorization_blocked",
            Self::RecoveryPrepared { .. } => "recovery_prepared",
            Self::ModelRequestStarted { .. } => "model_request_started",
            Self::ModelRequestFinished { .. } => "model_request_finished",
            Self::ModelResponseFinished { .. } => "model_response_finished",
            Self::ToolExecutionStarted { .. } => "tool_execution_started",
            Self::ToolExecutionFinished { .. } => "tool_execution_finished",
            Self::PolicyDenied { .. } => "policy_denied",
            Self::BackgroundTaskStarted { .. } => "background_task_started",
            Self::BackgroundTaskFinished { .. } => "background_task_finished",
            Self::MemorySearchStarted { .. } => "memory_search_started",
            Self::MemorySearchFinished { .. } => "memory_search_finished",
            Self::MemoryIngestStarted { .. } => "memory_ingest_started",
            Self::MemoryIngestFinished { .. } => "memory_ingest_finished",
            Self::MemoryCompactionProposed { .. } => "memory_compaction_proposed",
            Self::MemoryCompactionApplied { .. } => "memory_compaction_applied",
            Self::MemoryCompactionSkipped { .. } => "memory_compaction_skipped",
            Self::RunAborted { .. } => "run_aborted",
            Self::ToolExecutionBlocked { .. } => "tool_execution_blocked",
        }
    }
}

pub trait RuntimeHook: Send + Sync {
    fn on_event(
        &self,
        store: &dyn AuditStore,
        event: &RuntimeHookEvent,
    ) -> Result<(), RuntimeError>;
}

pub struct AuditHook;
pub type AuditLogHook = AuditHook;

impl RuntimeHook for AuditHook {
    fn on_event(
        &self,
        store: &dyn AuditStore,
        event: &RuntimeHookEvent,
    ) -> Result<(), RuntimeError> {
        store.record_audit_event(
            &event.scope(),
            event.event_type(),
            serde_json::to_value(event).map_err(|error| RuntimeError::Store(error.to_string()))?,
        )
    }
}

#[derive(Clone, Default)]
pub struct RuntimeHooks {
    hooks: Vec<Arc<dyn RuntimeHook>>,
}

impl RuntimeHooks {
    pub fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    pub fn with_hook<H>(mut self, hook: H) -> Self
    where
        H: RuntimeHook + 'static,
    {
        self.hooks.push(Arc::new(hook));
        self
    }

    pub fn extend<I>(mut self, hooks: I) -> Self
    where
        I: IntoIterator<Item = Arc<dyn RuntimeHook>>,
    {
        self.hooks.extend(hooks);
        self
    }

    pub fn emit(
        &self,
        store: &dyn AuditStore,
        event: &RuntimeHookEvent,
    ) -> Result<(), RuntimeError> {
        for hook in &self.hooks {
            hook.on_event(store, event)?;
        }
        Ok(())
    }

    pub(crate) fn emit_runtime(
        &self,
        store: &dyn RuntimeStore,
        event: &RuntimeHookEvent,
    ) -> Result<(), RuntimeError> {
        self.emit(&RuntimeAuditStore(store), event)
    }
}

/// Stable adapter for Rust versions that cannot upcast `dyn RuntimeStore` to
/// its `AuditStore` supertrait object directly.
struct RuntimeAuditStore<'a>(&'a dyn RuntimeStore);

impl AuditStore for RuntimeAuditStore<'_> {
    fn record_audit_event(
        &self,
        scope: &str,
        event_type: &str,
        payload: serde_json::Value,
    ) -> Result<(), RuntimeError> {
        self.0.record_audit_event(scope, event_type, payload)
    }
}

// ---------------------------------------------------------------------------
// Pre-execution hook types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PreExecutionContext {
    pub agent_id: String,
    pub tool_name: String,
    pub tool_call_id: String,
    pub input_json: String,
    /// What a relative path in `input_json` resolves against.
    ///
    /// A hook inspecting `{"path": "../../etc/hosts"}` cannot judge it without
    /// knowing where it starts from, and guessing the workspace root is only
    /// right until it isn't.
    pub working_directory: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDecision {
    Allow,
    Deny(String),
    /// Run the tool with this input instead of the one the model produced.
    ///
    /// For the cases a veto answers badly: redacting a secret out of an
    /// argument, normalizing a path against the right root, narrowing an
    /// over-broad command. Denying those costs a round trip and often does not
    /// converge, because the model is told "no" without being told what would
    /// have been acceptable.
    ///
    /// The replacement is re-checked by every remaining hook, so a later hook
    /// still sees — and can still refuse — what an earlier one produced. A hook
    /// cannot use `Modify` to smuggle a call past a hook that runs after it.
    ///
    /// Nor past anything that runs after the hook chain. The replacement is
    /// validated against the tool's `input_schema` and is what the
    /// [`ToolAuthorizer`](crate::tool::ToolAuthorizer) is asked about, so a
    /// remembered permission rule is written against — and matched against —
    /// the input that actually runs. A replacement that does not fit the
    /// schema is refused as the hook's failure, with the tool never entered
    /// and no one asked for permission.
    Modify {
        /// The tool's new input, as JSON.
        input_json: String,
        /// Why, for the audit trail.
        reason: Option<String>,
    },
}

/// Consulted before a tool runs, and able to stop or rewrite the call.
///
/// Async because it is invoked from inside a turn: a hook that reads a file,
/// spawns a process, or asks a service would otherwise block a runtime worker
/// for its whole duration. A synchronous signature left every implementor to
/// discover that `tokio::task::block_in_place` panics on a current_thread
/// runtime and to branch on `Handle::runtime_flavor()` themselves.
///
/// The same shape as [`ToolAuthorizer`](crate::tool::ToolAuthorizer), which
/// sits at the adjacent seam doing the same kind of work.
///
/// # Order of the seams
///
/// A scheduled call meets its gates in this order, on both the serial and the
/// parallel execution lane:
///
/// 1. every pre-execution hook, in registration order;
/// 2. the tool's `input_schema` check, against whatever the hooks left;
/// 3. the [`ToolAuthorizer`](crate::tool::ToolAuthorizer).
///
/// Hooks run first so that the authorizer — and any permission rule a host
/// remembers from its answer — judges the call that will actually run. A hook
/// that narrows an over-broad command makes the narrowed command the thing a
/// person approves, which is the point of narrowing it. Two consequences a
/// host can rely on:
///
/// - A [`HookDecision::Deny`] short-circuits before the authorizer is
///   consulted; the call is answered as blocked by the hook and nobody is
///   asked.
/// - A hook runs for every registered call, including one the authorizer would
///   have refused. A hook with side effects sees calls it did not before, and
///   must not assume the call it inspects has been approved.
#[async_trait]
pub trait PreExecutionHook: Send + Sync {
    async fn pre_tool_execution(
        &self,
        context: &PreExecutionContext,
    ) -> Result<HookDecision, RuntimeError>;
}

/// Forwards to the hook inside.
///
/// Lets a caller hold a hook it chose at runtime — one of several, or none —
/// and still hand it to anything taking `impl PreExecutionHook`, without each
/// caller writing this impl itself. The same courtesy `ToolAuthorizer` gets.
#[async_trait]
impl<T: PreExecutionHook + ?Sized> PreExecutionHook for Box<T> {
    async fn pre_tool_execution(
        &self,
        context: &PreExecutionContext,
    ) -> Result<HookDecision, RuntimeError> {
        (**self).pre_tool_execution(context).await
    }
}

#[async_trait]
impl<T: PreExecutionHook + ?Sized> PreExecutionHook for Arc<T> {
    async fn pre_tool_execution(
        &self,
        context: &PreExecutionContext,
    ) -> Result<HookDecision, RuntimeError> {
        (**self).pre_tool_execution(context).await
    }
}

/// Keeps one live pre-execution hook registered.
///
/// Dropping the guard, or consuming it with [`unregister`](Self::unregister),
/// removes only this exact registration. An invocation that already snapshotted
/// the hook may still finish. The guard does not keep its runtime alive.
#[must_use = "dropping the guard immediately unregisters the pre-execution hook"]
pub struct PreExecutionHookRegistration {
    inner: LiveHookRegistration<Arc<dyn PreExecutionHook>>,
}

impl fmt::Debug for PreExecutionHookRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreExecutionHookRegistration")
            .field("audience", &self.inner.audience)
            .field("active", &self.inner.active)
            .finish_non_exhaustive()
    }
}

impl PreExecutionHookRegistration {
    /// Returns the audience this hook is scoped to, or `None` when it is global.
    pub fn audience(&self) -> Option<&ToolAudience> {
        self.inner.audience.as_ref()
    }

    /// Unregisters this exact hook now.
    pub fn unregister(mut self) -> bool {
        self.inner.unregister()
    }
}

/// Keeps one caller-keyed pre-execution hook registered while any holder lives.
///
/// Cloning this guard, or registering the same key with the same audience and
/// [`Arc`] allocation again, creates another holder without another chain
/// entry. The last holder to be dropped removes the exact entry it represents.
/// Shared keys are local to the pre-execution chain; post and mixed hooks have
/// independent key namespaces.
#[derive(Clone)]
#[must_use = "dropping the last holder unregisters the shared pre-execution hook"]
pub struct SharedPreExecutionHookRegistration {
    inner: Arc<SharedLiveHookRegistration<Arc<dyn PreExecutionHook>>>,
}

impl fmt::Debug for SharedPreExecutionHookRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SharedPreExecutionHookRegistration")
            .field("key", &self.inner.key)
            .field("audience", &self.inner.audience)
            .finish_non_exhaustive()
    }
}

impl SharedPreExecutionHookRegistration {
    /// Returns the caller-supplied identity key for this shared entry.
    pub fn key(&self) -> &str {
        &self.inner.key
    }

    /// Returns the audience this hook is scoped to, or `None` when it is global.
    pub fn audience(&self) -> Option<&ToolAudience> {
        self.inner.audience.as_ref()
    }
}

pub(crate) struct PreExecutionHookSnapshot {
    hooks: Vec<Arc<dyn PreExecutionHook>>,
}

impl PreExecutionHookSnapshot {
    /// Runs every snapshotted hook in order, threading modifications through.
    pub(crate) async fn run(
        &self,
        context: &PreExecutionContext,
    ) -> Result<HookDecision, RuntimeError> {
        let mut current = context.clone();
        let mut modified = None;

        for hook in &self.hooks {
            match hook.pre_tool_execution(&current).await? {
                HookDecision::Allow => continue,
                deny @ HookDecision::Deny(_) => return Ok(deny),
                HookDecision::Modify { input_json, reason } => {
                    current.input_json = input_json.clone();
                    modified = Some(HookDecision::Modify { input_json, reason });
                }
            }
        }

        Ok(modified.unwrap_or(HookDecision::Allow))
    }
}

#[derive(Clone)]
pub struct PreExecutionHooks {
    hooks: Vec<Arc<dyn PreExecutionHook>>,
    live: Arc<RwLock<LiveHookRegistry<Arc<dyn PreExecutionHook>>>>,
}

impl Default for PreExecutionHooks {
    fn default() -> Self {
        Self::new()
    }
}

impl PreExecutionHooks {
    pub fn new() -> Self {
        Self {
            hooks: Vec::new(),
            live: Arc::new(RwLock::new(LiveHookRegistry::new())),
        }
    }

    pub fn with_hook<H>(mut self, hook: H) -> Self
    where
        H: PreExecutionHook + 'static,
    {
        self.hooks.push(Arc::new(hook));
        self
    }

    pub(crate) fn register_live<H>(
        &self,
        audience: Option<ToolAudience>,
        hook: H,
    ) -> PreExecutionHookRegistration
    where
        H: PreExecutionHook + 'static,
    {
        let id = next_execution_hook_registration_id();
        let guard_audience = audience.clone();
        let hook: Arc<dyn PreExecutionHook> = Arc::new(hook);
        self.live
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, audience, hook);
        PreExecutionHookRegistration {
            inner: LiveHookRegistration::new(Arc::downgrade(&self.live), id, guard_audience),
        }
    }

    pub(crate) fn register_live_shared(
        &self,
        key: String,
        audience: Option<ToolAudience>,
        hook: Arc<dyn PreExecutionHook>,
    ) -> Result<SharedPreExecutionHookRegistration, SharedHookRegistrationConflict> {
        let id = next_execution_hook_registration_id();
        let inner =
            LiveHookRegistry::register_shared(&self.live, id, key, audience, hook, Arc::ptr_eq)?;
        Ok(SharedPreExecutionHookRegistration { inner })
    }

    pub(crate) fn snapshot(&self, audience: Option<&ToolAudience>) -> PreExecutionHookSnapshot {
        let live = self
            .live
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut hooks = Vec::with_capacity(self.hooks.len() + live.entries.len());
        hooks.extend(self.hooks.iter().cloned());
        hooks.extend(live.matching(audience).cloned());
        PreExecutionHookSnapshot { hooks }
    }

    /// Runs every hook in order, threading any modification through the rest.
    ///
    /// Returns the surviving decision: a `Deny` from any hook short-circuits,
    /// and otherwise the last `Modify` (if any) is what the tool should run
    /// with. Each hook sees the input as its predecessors left it, so
    /// modifications compose and no hook can route a call around a later one.
    pub async fn run(&self, context: &PreExecutionContext) -> Result<HookDecision, RuntimeError> {
        self.snapshot(None).run(context).await
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
            && self
                .live
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
    }
}

/// Returns whether a provider error is likely transient and worth retrying.
pub fn is_transient_provider_error(error: &ProviderError) -> bool {
    match error {
        ProviderError::Transport(_)
        | ProviderError::Decode(_)
        | ProviderError::Retryable { .. } => true,
        ProviderError::Http { status, .. } => {
            status.is_server_error()
                || *status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || *status == reqwest::StatusCode::REQUEST_TIMEOUT
        }
        // Not transient: the same request will be too long every time. The
        // runtime recovers from this one by compacting and trying again, not
        // by waiting.
        ProviderError::ContextLengthExceeded { .. }
        | ProviderError::Serialize(_)
        | ProviderError::Deserialize(_)
        | ProviderError::InvalidRequest(_)
        | ProviderError::InvalidResponse(_)
        | ProviderError::MalformedStream(_)
        | ProviderError::UnsupportedCapability(_) => false,
    }
}

/// Returns whether a runtime error is backed by a transient provider failure.
///
/// Delegates to [`RuntimeError::category()`] so there is a single source of
/// truth for error classification.
pub fn is_transient_runtime_error(error: &RuntimeError) -> bool {
    error.category() == crate::error::ErrorCategory::Retryable
}

pub(super) struct LiveHookEntry<T> {
    id: u64,
    audience: Option<ToolAudience>,
    value: T,
    shared: Option<SharedHookEntry<T>>,
}

struct SharedHookEntry<T> {
    key: String,
    registration: Weak<SharedLiveHookRegistration<T>>,
}

pub(super) struct LiveHookRegistry<T> {
    entries: Vec<LiveHookEntry<T>>,
}

impl<T> LiveHookRegistry<T> {
    pub(super) fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub(super) fn insert(&mut self, id: u64, audience: Option<ToolAudience>, value: T) {
        self.entries.push(LiveHookEntry {
            id,
            audience,
            value,
            shared: None,
        });
    }

    pub(super) fn register_shared<F>(
        registry: &Arc<RwLock<Self>>,
        id: u64,
        key: String,
        audience: Option<ToolAudience>,
        value: T,
        same_value: F,
    ) -> Result<Arc<SharedLiveHookRegistration<T>>, SharedHookRegistrationConflict>
    where
        F: Fn(&T, &T) -> bool,
    {
        let mut value = Some(value);
        let mut stale_entry = None;
        let mut existing_registration = None;
        let mut conflict = false;
        let mut inserted_registration = None;

        {
            let mut registry_guard = registry
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let matching_index = registry_guard.entries.iter().position(|entry| {
                entry
                    .shared
                    .as_ref()
                    .is_some_and(|shared| shared.key == key)
            });

            if let Some(index) = matching_index {
                let live_registration = registry_guard.entries[index]
                    .shared
                    .as_ref()
                    .and_then(|shared| shared.registration.upgrade());
                if let Some(registration) = live_registration {
                    let entry = &registry_guard.entries[index];
                    if entry.audience == audience
                        && same_value(&entry.value, value.as_ref().expect("shared value"))
                    {
                        existing_registration = Some(registration);
                    } else {
                        conflict = true;
                    }
                } else {
                    stale_entry = Some(registry_guard.entries.remove(index));
                }
            }

            if !conflict && existing_registration.is_none() {
                let registration = Arc::new(SharedLiveHookRegistration {
                    registry: Arc::downgrade(registry),
                    id,
                    key: key.clone(),
                    audience: audience.clone(),
                });
                registry_guard.entries.push(LiveHookEntry {
                    id,
                    audience,
                    value: value.take().expect("shared value inserted once"),
                    shared: Some(SharedHookEntry {
                        key: key.clone(),
                        registration: Arc::downgrade(&registration),
                    }),
                });
                inserted_registration = Some(registration);
            }
        }

        // Hook captures may register another hook from Drop, so neither a
        // replaced stale entry nor a rejected duplicate is destroyed while
        // the registry is locked.
        drop(stale_entry);
        drop(value);

        if conflict {
            Err(SharedHookRegistrationConflict { key })
        } else {
            Ok(existing_registration
                .or(inserted_registration)
                .expect("shared registration outcome"))
        }
    }

    pub(super) fn matching<'a>(
        &'a self,
        audience: Option<&'a ToolAudience>,
    ) -> impl Iterator<Item = &'a T> + 'a {
        self.entries
            .iter()
            .filter(move |entry| match &entry.audience {
                None => true,
                Some(expected) => audience == Some(expected),
            })
            .map(|entry| &entry.value)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(super) fn detach(&mut self, id: u64) -> Option<LiveHookEntry<T>> {
        let index = self.entries.iter().position(|entry| entry.id == id)?;
        Some(self.entries.remove(index))
    }
}

pub(super) struct LiveHookRegistration<T> {
    pub(super) registry: Weak<RwLock<LiveHookRegistry<T>>>,
    id: u64,
    pub(super) audience: Option<ToolAudience>,
    pub(super) active: bool,
}

impl<T> LiveHookRegistration<T> {
    pub(super) fn new(
        registry: Weak<RwLock<LiveHookRegistry<T>>>,
        id: u64,
        audience: Option<ToolAudience>,
    ) -> Self {
        Self {
            registry,
            id,
            audience,
            active: true,
        }
    }

    pub(super) fn unregister(&mut self) -> bool {
        if !self.active {
            return false;
        }
        self.active = false;
        let Some(registry) = self.registry.upgrade() else {
            return false;
        };
        let detached_entry = {
            let mut registry = registry
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            registry.detach(self.id)
        };
        let removed = detached_entry.is_some();
        drop(detached_entry);
        removed
    }
}

impl<T> Drop for LiveHookRegistration<T> {
    fn drop(&mut self) {
        self.unregister();
    }
}

pub(super) struct SharedLiveHookRegistration<T> {
    registry: Weak<RwLock<LiveHookRegistry<T>>>,
    id: u64,
    pub(super) key: String,
    pub(super) audience: Option<ToolAudience>,
}

impl<T> Drop for SharedLiveHookRegistration<T> {
    fn drop(&mut self) {
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        let detached_entry = {
            let mut registry = registry
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            registry.detach(self.id)
        };
        drop(detached_entry);
    }
}

/// A caller reused an active shared-hook key with a different registration.
///
/// Reusing a key succeeds only when the audience and exact [`Arc`] hook (or
/// ordered mixed-hook batch) are unchanged. Key namespaces are independent for
/// pre-execution, post-execution, and mixed execution hooks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedHookRegistrationConflict {
    key: String,
}

impl SharedHookRegistrationConflict {
    /// Returns the caller-supplied key that conflicted.
    pub fn key(&self) -> &str {
        &self.key
    }
}

impl fmt::Display for SharedHookRegistrationConflict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "shared hook key '{}' is already registered with a different hook or audience",
            self.key
        )
    }
}

impl std::error::Error for SharedHookRegistrationConflict {}

// ---------------------------------------------------------------------------
// Post-execution hook types
// ---------------------------------------------------------------------------

/// What a tool produced, handed to the hooks before the model sees it.
#[derive(Debug, Clone)]
pub struct PostExecutionContext {
    pub agent_id: String,
    pub tool_name: String,
    pub tool_call_id: String,
    /// The input the tool actually ran with, as JSON — after any
    /// [`HookDecision::Modify`] a pre-execution hook applied, not the input the
    /// model wrote.
    pub input_json: String,
    /// What a relative path in the result resolves against, as for
    /// [`PreExecutionContext`].
    pub working_directory: PathBuf,
    /// What the tool returned.
    pub content: ToolResultContent,
    /// Whether the tool reported failure.
    pub is_error: bool,
}

/// What should reach the model in place of what the tool returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultDecision {
    /// Leave the result as it is.
    Keep,
    /// Show the model this instead.
    ///
    /// The seam a pre-execution hook cannot cover: whether a call is acceptable
    /// is often only answerable from its *output* — a grep that pulled a
    /// credential out of a file nobody meant to expose, a command whose stderr
    /// carries an internal hostname, a result worth annotating rather than
    /// hiding. Denying the call up front cannot express any of it, because up
    /// front the output does not exist yet.
    Replace {
        content: ToolResultContent,
        is_error: bool,
    },
}

/// Consulted after a tool runs, and able to rewrite what the model is shown.
///
/// Async for the same reason [`PreExecutionHook`] is: it runs inside a turn,
/// and a hook that reads a file or asks a service would otherwise block a
/// runtime worker for its whole duration.
///
/// A hook cannot un-run the tool. By the time it is consulted the side effects
/// have happened and `AgentEvent::ToolExecutionFinished` has already carried
/// the unmodified result to every subscriber — this seam governs the model's
/// view of the result, not the audit trail of what actually occurred.
#[async_trait]
pub trait PostExecutionHook: Send + Sync {
    async fn post_tool_execution(
        &self,
        context: &PostExecutionContext,
    ) -> Result<ResultDecision, RuntimeError>;
}

/// Forwards to the hook inside, as [`PreExecutionHook`] does.
#[async_trait]
impl<T: PostExecutionHook + ?Sized> PostExecutionHook for Box<T> {
    async fn post_tool_execution(
        &self,
        context: &PostExecutionContext,
    ) -> Result<ResultDecision, RuntimeError> {
        (**self).post_tool_execution(context).await
    }
}

#[async_trait]
impl<T: PostExecutionHook + ?Sized> PostExecutionHook for Arc<T> {
    async fn post_tool_execution(
        &self,
        context: &PostExecutionContext,
    ) -> Result<ResultDecision, RuntimeError> {
        (**self).post_tool_execution(context).await
    }
}

/// Keeps one live post-execution hook registered.
///
/// Dropping the guard, or consuming it with [`unregister`](Self::unregister),
/// removes only this exact registration. An invocation that already snapshotted
/// the hook may still finish. The guard does not keep its runtime alive.
#[must_use = "dropping the guard immediately unregisters the post-execution hook"]
pub struct PostExecutionHookRegistration {
    inner: LiveHookRegistration<Arc<dyn PostExecutionHook>>,
}

impl fmt::Debug for PostExecutionHookRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PostExecutionHookRegistration")
            .field("audience", &self.inner.audience)
            .field("active", &self.inner.active)
            .finish_non_exhaustive()
    }
}

impl PostExecutionHookRegistration {
    /// Returns the audience this hook is scoped to, or `None` when it is global.
    pub fn audience(&self) -> Option<&ToolAudience> {
        self.inner.audience.as_ref()
    }

    /// Unregisters this exact hook now.
    pub fn unregister(mut self) -> bool {
        self.inner.unregister()
    }
}

/// Keeps one caller-keyed post-execution hook registered while any holder lives.
///
/// The same key, audience, and [`Arc`] allocation share one chain entry. The
/// last holder to be dropped removes that entry. Keys are local to the
/// post-execution chain.
#[derive(Clone)]
#[must_use = "dropping the last holder unregisters the shared post-execution hook"]
pub struct SharedPostExecutionHookRegistration {
    inner: Arc<SharedLiveHookRegistration<Arc<dyn PostExecutionHook>>>,
}

impl fmt::Debug for SharedPostExecutionHookRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SharedPostExecutionHookRegistration")
            .field("key", &self.inner.key)
            .field("audience", &self.inner.audience)
            .finish_non_exhaustive()
    }
}

impl SharedPostExecutionHookRegistration {
    /// Returns the caller-supplied identity key for this shared entry.
    pub fn key(&self) -> &str {
        &self.inner.key
    }

    /// Returns the audience this hook is scoped to, or `None` when it is global.
    pub fn audience(&self) -> Option<&ToolAudience> {
        self.inner.audience.as_ref()
    }
}

pub(crate) struct PostExecutionHookSnapshot {
    hooks: Vec<Arc<dyn PostExecutionHook>>,
}

impl PostExecutionHookSnapshot {
    pub(crate) fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Runs every snapshotted hook in reverse order, threading replacements.
    pub(crate) async fn run(
        &self,
        context: &PostExecutionContext,
    ) -> Result<ResultDecision, RuntimeError> {
        let mut current = context.clone();
        let mut replaced = ResultDecision::Keep;

        for hook in self.hooks.iter().rev() {
            if let ResultDecision::Replace { content, is_error } =
                hook.post_tool_execution(&current).await?
            {
                current.content = content.clone();
                current.is_error = is_error;
                replaced = ResultDecision::Replace { content, is_error };
            }
        }

        Ok(replaced)
    }
}

#[derive(Clone)]
pub struct PostExecutionHooks {
    hooks: Vec<Arc<dyn PostExecutionHook>>,
    live: Arc<RwLock<LiveHookRegistry<Arc<dyn PostExecutionHook>>>>,
}

impl Default for PostExecutionHooks {
    fn default() -> Self {
        Self::new()
    }
}

impl PostExecutionHooks {
    pub fn new() -> Self {
        Self {
            hooks: Vec::new(),
            live: Arc::new(RwLock::new(LiveHookRegistry::new())),
        }
    }

    pub fn with_hook<H>(mut self, hook: H) -> Self
    where
        H: PostExecutionHook + 'static,
    {
        self.hooks.push(Arc::new(hook));
        self
    }

    pub(crate) fn register_live<H>(
        &self,
        audience: Option<ToolAudience>,
        hook: H,
    ) -> PostExecutionHookRegistration
    where
        H: PostExecutionHook + 'static,
    {
        let id = next_execution_hook_registration_id();
        let guard_audience = audience.clone();
        let hook: Arc<dyn PostExecutionHook> = Arc::new(hook);
        self.live
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, audience, hook);
        PostExecutionHookRegistration {
            inner: LiveHookRegistration::new(Arc::downgrade(&self.live), id, guard_audience),
        }
    }

    pub(crate) fn register_live_shared(
        &self,
        key: String,
        audience: Option<ToolAudience>,
        hook: Arc<dyn PostExecutionHook>,
    ) -> Result<SharedPostExecutionHookRegistration, SharedHookRegistrationConflict> {
        let id = next_execution_hook_registration_id();
        let inner =
            LiveHookRegistry::register_shared(&self.live, id, key, audience, hook, Arc::ptr_eq)?;
        Ok(SharedPostExecutionHookRegistration { inner })
    }

    pub(crate) fn snapshot(&self, audience: Option<&ToolAudience>) -> PostExecutionHookSnapshot {
        let live = self
            .live
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut hooks = Vec::with_capacity(self.hooks.len() + live.entries.len());
        hooks.extend(self.hooks.iter().cloned());
        hooks.extend(live.matching(audience).cloned());
        PostExecutionHookSnapshot { hooks }
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
            && self
                .live
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
    }

    /// Runs every hook in reverse registration order, threading each
    /// replacement into the next.
    ///
    /// Reverse, where the pre-execution hooks run forward, because the two
    /// bracket the call: the hooks a host registers first are the outermost,
    /// so they see the input first on the way in and the result last on the
    /// way out. A guard registered before another therefore has the final say
    /// in both directions, which is the only ordering under which "registered
    /// first, so it wraps everything after it" holds.
    ///
    /// Each hook sees the result as its predecessors left it, so no hook can
    /// slip a result past one that wraps it.
    pub async fn run(
        &self,
        context: &PostExecutionContext,
    ) -> Result<ResultDecision, RuntimeError> {
        self.snapshot(None).run(context).await
    }
}

#[cfg(test)]
mod post_execution {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use async_trait::async_trait;

    use crate::runtime::control::hooks::{
        PostExecutionContext, PostExecutionHook, PostExecutionHooks, ResultDecision,
    };
    use crate::{error::RuntimeError, tool::ToolResultContent};

    fn context(content: &str) -> PostExecutionContext {
        PostExecutionContext {
            agent_id: "agent".to_string(),
            tool_name: "grep".to_string(),
            tool_call_id: "call-1".to_string(),
            input_json: r#"{"pattern":"token"}"#.to_string(),
            working_directory: std::path::PathBuf::from("/repo"),
            content: ToolResultContent::text(content),
            is_error: false,
        }
    }

    struct Appending(&'static str);

    #[async_trait]
    impl PostExecutionHook for Appending {
        async fn post_tool_execution(
            &self,
            context: &PostExecutionContext,
        ) -> Result<ResultDecision, RuntimeError> {
            Ok(ResultDecision::Replace {
                content: ToolResultContent::text(format!(
                    "{}{}",
                    context.content.to_display_string(),
                    self.0
                )),
                is_error: context.is_error,
            })
        }
    }

    struct Keeps(Arc<AtomicUsize>);

    #[async_trait]
    impl PostExecutionHook for Keeps {
        async fn post_tool_execution(
            &self,
            _context: &PostExecutionContext,
        ) -> Result<ResultDecision, RuntimeError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ResultDecision::Keep)
        }
    }

    #[tokio::test]
    async fn no_hooks_keeps_the_result() {
        let decision = PostExecutionHooks::new()
            .run(&context("out"))
            .await
            .expect("hooks run");

        assert_eq!(decision, ResultDecision::Keep);
    }

    #[tokio::test]
    async fn a_hook_that_keeps_leaves_the_result_alone() {
        let seen = Arc::new(AtomicUsize::new(0));
        let decision = PostExecutionHooks::new()
            .with_hook(Keeps(Arc::clone(&seen)))
            .run(&context("out"))
            .await
            .expect("hooks run");

        assert_eq!(decision, ResultDecision::Keep);
        assert_eq!(seen.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn hooks_run_in_reverse_registration_order() {
        // The pre hooks run forward and these run backward because the two
        // bracket the call: whoever was registered first is outermost, so
        // it sees the input first and the result last.
        let decision = PostExecutionHooks::new()
            .with_hook(Appending("-outer"))
            .with_hook(Appending("-inner"))
            .run(&context("out"))
            .await
            .expect("hooks run");

        let ResultDecision::Replace { content, .. } = decision else {
            panic!("expected a replacement");
        };
        assert_eq!(content.to_display_string(), "out-inner-outer");
    }

    #[tokio::test]
    async fn a_later_hook_sees_what_an_earlier_one_produced() {
        struct Requires;

        #[async_trait]
        impl PostExecutionHook for Requires {
            async fn post_tool_execution(
                &self,
                context: &PostExecutionContext,
            ) -> Result<ResultDecision, RuntimeError> {
                assert_eq!(
                    context.content.to_display_string(),
                    "out-inner",
                    "the outer hook must see the inner hook's replacement"
                );
                Ok(ResultDecision::Keep)
            }
        }

        let decision = PostExecutionHooks::new()
            .with_hook(Requires)
            .with_hook(Appending("-inner"))
            .run(&context("out"))
            .await
            .expect("hooks run");

        let ResultDecision::Replace { content, .. } = decision else {
            panic!("a keep after a replace must not discard the replacement");
        };
        assert_eq!(content.to_display_string(), "out-inner");
    }

    #[tokio::test]
    async fn a_hook_can_turn_a_success_into_an_error() {
        struct Fails;

        #[async_trait]
        impl PostExecutionHook for Fails {
            async fn post_tool_execution(
                &self,
                _context: &PostExecutionContext,
            ) -> Result<ResultDecision, RuntimeError> {
                Ok(ResultDecision::Replace {
                    content: ToolResultContent::text("refused: result held a credential"),
                    is_error: true,
                })
            }
        }

        let decision = PostExecutionHooks::new()
            .with_hook(Fails)
            .run(&context("AKIA..."))
            .await
            .expect("hooks run");

        assert_eq!(
            decision,
            ResultDecision::Replace {
                content: ToolResultContent::text("refused: result held a credential"),
                is_error: true,
            }
        );
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn make_context(tool_name: &str) -> PreExecutionContext {
        PreExecutionContext {
            agent_id: "agent-1".to_string(),
            tool_name: tool_name.to_string(),
            tool_call_id: "call-1".to_string(),
            input_json: "{}".to_string(),
            working_directory: PathBuf::from("/repo"),
        }
    }

    struct AllowHook;
    #[async_trait]
    impl PreExecutionHook for AllowHook {
        async fn pre_tool_execution(
            &self,
            _context: &PreExecutionContext,
        ) -> Result<HookDecision, RuntimeError> {
            Ok(HookDecision::Allow)
        }
    }

    struct DenyHook;
    #[async_trait]
    impl PreExecutionHook for DenyHook {
        async fn pre_tool_execution(
            &self,
            _context: &PreExecutionContext,
        ) -> Result<HookDecision, RuntimeError> {
            Ok(HookDecision::Deny("denied by DenyHook".to_string()))
        }
    }

    struct ToolNameDenyHook {
        blocked_tool: String,
    }
    #[async_trait]
    impl PreExecutionHook for ToolNameDenyHook {
        async fn pre_tool_execution(
            &self,
            context: &PreExecutionContext,
        ) -> Result<HookDecision, RuntimeError> {
            if context.tool_name == self.blocked_tool {
                Ok(HookDecision::Deny(format!(
                    "tool '{}' is blocked",
                    context.tool_name
                )))
            } else {
                Ok(HookDecision::Allow)
            }
        }
    }

    #[tokio::test]
    async fn empty_pre_hooks_allows() {
        let hooks = PreExecutionHooks::new();
        let result = hooks.run(&make_context("shell")).await.unwrap();
        assert_eq!(result, HookDecision::Allow);
    }

    #[tokio::test]
    async fn all_allow_hooks_allows() {
        let hooks = PreExecutionHooks::new()
            .with_hook(AllowHook)
            .with_hook(AllowHook);
        let result = hooks.run(&make_context("files")).await.unwrap();
        assert_eq!(result, HookDecision::Allow);
    }

    #[tokio::test]
    async fn first_deny_wins() {
        let hooks = PreExecutionHooks::new()
            .with_hook(AllowHook)
            .with_hook(DenyHook)
            .with_hook(AllowHook);
        let result = hooks.run(&make_context("any_tool")).await.unwrap();
        assert_eq!(result, HookDecision::Deny("denied by DenyHook".to_string()));
    }

    #[tokio::test]
    async fn conditional_deny_by_tool_name() {
        let hooks = PreExecutionHooks::new().with_hook(ToolNameDenyHook {
            blocked_tool: "shell".to_string(),
        });

        let shell_result = hooks.run(&make_context("shell")).await.unwrap();
        assert_eq!(
            shell_result,
            HookDecision::Deny("tool 'shell' is blocked".to_string())
        );

        let files_result = hooks.run(&make_context("files")).await.unwrap();
        assert_eq!(files_result, HookDecision::Allow);
    }

    fn http(status: reqwest::StatusCode, retry_after: Option<Duration>) -> ProviderError {
        ProviderError::Http {
            status,
            body: String::new(),
            retry_after,
        }
    }

    #[test]
    fn a_rate_limit_is_transient_whether_or_not_it_named_a_window() {
        // Classification is what decides a retry happens at all; the schedule
        // only decides how long it waits. A `Retry-After` must not change the
        // first answer, in either direction.
        for retry_after in [None, Some(Duration::from_secs(45))] {
            assert!(is_transient_provider_error(&http(
                reqwest::StatusCode::TOO_MANY_REQUESTS,
                retry_after
            )));
            assert!(is_transient_provider_error(&http(
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                retry_after
            )));
        }

        assert!(
            !is_transient_provider_error(&http(reqwest::StatusCode::BAD_REQUEST, None)),
            "a request the caller must fix is not worth re-sending"
        );
    }
}

#[cfg(test)]
mod pre_execution_tests {
    use super::*;

    struct Fixed(HookDecision);

    #[async_trait]
    impl PreExecutionHook for Fixed {
        async fn pre_tool_execution(
            &self,
            _context: &PreExecutionContext,
        ) -> Result<HookDecision, RuntimeError> {
            Ok(self.0.clone())
        }
    }

    /// Rewrites the input to whatever it last saw, prefixed — so a second
    /// modification proves it observed the first.
    struct Appending(&'static str);

    #[async_trait]
    impl PreExecutionHook for Appending {
        async fn pre_tool_execution(
            &self,
            context: &PreExecutionContext,
        ) -> Result<HookDecision, RuntimeError> {
            Ok(HookDecision::Modify {
                input_json: format!("{}{}", context.input_json, self.0),
                reason: None,
            })
        }
    }

    fn context() -> PreExecutionContext {
        PreExecutionContext {
            agent_id: "a1".to_string(),
            tool_name: "shell".to_string(),
            tool_call_id: "tc-1".to_string(),
            input_json: "start".to_string(),
            working_directory: PathBuf::from("/repo"),
        }
    }

    #[tokio::test]
    async fn no_hooks_allows() {
        let hooks = PreExecutionHooks::new();
        assert_eq!(hooks.run(&context()).await.unwrap(), HookDecision::Allow);
    }

    #[tokio::test]
    async fn a_deny_short_circuits_the_rest() {
        let hooks = PreExecutionHooks::new()
            .with_hook(Fixed(HookDecision::Deny("no".to_string())))
            .with_hook(Appending("-never"));

        assert_eq!(
            hooks.run(&context()).await.unwrap(),
            HookDecision::Deny("no".to_string()),
            "a hook after a denial must not get to overwrite the answer"
        );
    }

    #[tokio::test]
    async fn modifications_compose_in_order() {
        let hooks = PreExecutionHooks::new()
            .with_hook(Appending("-one"))
            .with_hook(Appending("-two"));

        let HookDecision::Modify { input_json, .. } = hooks.run(&context()).await.unwrap() else {
            panic!("expected a modification");
        };
        assert_eq!(
            input_json, "start-one-two",
            "each hook must see the input as its predecessors left it"
        );
    }

    #[tokio::test]
    async fn a_later_hook_can_still_deny_a_modified_call() {
        let hooks = PreExecutionHooks::new()
            .with_hook(Appending("-one"))
            .with_hook(Fixed(HookDecision::Deny("still no".to_string())));

        assert_eq!(
            hooks.run(&context()).await.unwrap(),
            HookDecision::Deny("still no".to_string()),
            "modify must not be a way around a hook that runs later"
        );
    }

    /// Awaits before answering, which is the whole reason the trait is async:
    /// under the old sync signature this had to be `block_in_place`, and that
    /// panics on a current_thread runtime.
    struct Awaits;

    #[async_trait]
    impl PreExecutionHook for Awaits {
        async fn pre_tool_execution(
            &self,
            _context: &PreExecutionContext,
        ) -> Result<HookDecision, RuntimeError> {
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            Ok(HookDecision::Deny("after awaiting".to_string()))
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_hook_may_await_even_on_a_current_thread_runtime() {
        let hooks = PreExecutionHooks::new().with_hook(Awaits);

        assert_eq!(
            hooks.run(&context()).await.unwrap(),
            HookDecision::Deny("after awaiting".to_string()),
            "a hook doing real work must not need a multi-thread runtime"
        );
    }
}

#[cfg(test)]
mod live_registration_tests {
    use std::{
        panic::AssertUnwindSafe,
        sync::{
            Arc, Barrier, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };

    use tokio::sync::Notify;

    use super::*;

    fn pre_context() -> PreExecutionContext {
        PreExecutionContext {
            agent_id: "agent".to_string(),
            tool_name: "echo".to_string(),
            tool_call_id: "call".to_string(),
            input_json: "{}".to_string(),
            working_directory: PathBuf::from("/same/root"),
        }
    }

    fn post_context() -> PostExecutionContext {
        PostExecutionContext {
            agent_id: "agent".to_string(),
            tool_name: "echo".to_string(),
            tool_call_id: "call".to_string(),
            input_json: "{}".to_string(),
            working_directory: PathBuf::from("/same/root"),
            content: ToolResultContent::text("out"),
            is_error: false,
        }
    }

    struct Records {
        label: &'static str,
        log: Arc<Mutex<Vec<&'static str>>>,
    }

    struct Allows;

    #[async_trait]
    impl PreExecutionHook for Allows {
        async fn pre_tool_execution(
            &self,
            _context: &PreExecutionContext,
        ) -> Result<HookDecision, RuntimeError> {
            Ok(HookDecision::Allow)
        }
    }

    #[async_trait]
    impl PreExecutionHook for Records {
        async fn pre_tool_execution(
            &self,
            _context: &PreExecutionContext,
        ) -> Result<HookDecision, RuntimeError> {
            self.log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(self.label);
            Ok(HookDecision::Allow)
        }
    }

    #[async_trait]
    impl PostExecutionHook for Records {
        async fn post_tool_execution(
            &self,
            _context: &PostExecutionContext,
        ) -> Result<ResultDecision, RuntimeError> {
            self.log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(self.label);
            Ok(ResultDecision::Keep)
        }
    }

    fn recorded(log: &Arc<Mutex<Vec<&'static str>>>) -> Vec<&'static str> {
        log.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn clear(log: &Arc<Mutex<Vec<&'static str>>>) {
        log.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    #[tokio::test]
    async fn builder_global_and_audience_hooks_share_one_filtered_order() {
        let alpha = ToolAudience::new("alpha");
        let beta = ToolAudience::new("beta");

        let pre_log = Arc::new(Mutex::new(Vec::new()));
        let pre = PreExecutionHooks::new().with_hook(Records {
            label: "builder",
            log: Arc::clone(&pre_log),
        });
        let _pre_global_one = pre.register_live(
            None,
            Records {
                label: "global-one",
                log: Arc::clone(&pre_log),
            },
        );
        let _pre_alpha = pre.register_live(
            Some(alpha.clone()),
            Records {
                label: "alpha",
                log: Arc::clone(&pre_log),
            },
        );
        let _pre_beta = pre.register_live(
            Some(beta.clone()),
            Records {
                label: "beta",
                log: Arc::clone(&pre_log),
            },
        );
        let _pre_global_two = pre.register_live(
            None,
            Records {
                label: "global-two",
                log: Arc::clone(&pre_log),
            },
        );

        pre.snapshot(Some(&alpha))
            .run(&pre_context())
            .await
            .expect("alpha pre hooks");
        assert_eq!(
            recorded(&pre_log),
            ["builder", "global-one", "alpha", "global-two"]
        );
        clear(&pre_log);
        pre.snapshot(None)
            .run(&pre_context())
            .await
            .expect("global pre hooks");
        assert_eq!(recorded(&pre_log), ["builder", "global-one", "global-two"]);

        let post_log = Arc::new(Mutex::new(Vec::new()));
        let post = PostExecutionHooks::new().with_hook(Records {
            label: "builder",
            log: Arc::clone(&post_log),
        });
        let _post_global_one = post.register_live(
            None,
            Records {
                label: "global-one",
                log: Arc::clone(&post_log),
            },
        );
        let _post_alpha = post.register_live(
            Some(alpha),
            Records {
                label: "alpha",
                log: Arc::clone(&post_log),
            },
        );
        let _post_beta = post.register_live(
            Some(beta),
            Records {
                label: "beta",
                log: Arc::clone(&post_log),
            },
        );
        let _post_global_two = post.register_live(
            None,
            Records {
                label: "global-two",
                log: Arc::clone(&post_log),
            },
        );

        post.snapshot(Some(&ToolAudience::new("alpha")))
            .run(&post_context())
            .await
            .expect("alpha post hooks");
        assert_eq!(
            recorded(&post_log),
            ["global-two", "alpha", "global-one", "builder"]
        );
        clear(&post_log);
        post.snapshot(None)
            .run(&post_context())
            .await
            .expect("global post hooks");
        assert_eq!(recorded(&post_log), ["global-two", "global-one", "builder"]);
    }

    #[tokio::test]
    async fn duplicate_guards_and_middle_removal_are_registration_exact() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let hooks = PreExecutionHooks::new();
        let duplicate = Arc::new(Records {
            label: "duplicate",
            log: Arc::clone(&log),
        });
        let left = hooks.register_live(
            None,
            Records {
                label: "left",
                log: Arc::clone(&log),
            },
        );
        let first = hooks.register_live(None, Arc::clone(&duplicate));
        let middle = hooks.register_live(
            None,
            Records {
                label: "middle",
                log: Arc::clone(&log),
            },
        );
        let last = hooks.register_live(None, duplicate);
        let right = hooks.register_live(
            None,
            Records {
                label: "right",
                log: Arc::clone(&log),
            },
        );

        drop(middle);
        hooks
            .snapshot(None)
            .run(&pre_context())
            .await
            .expect("two duplicates remain");
        assert_eq!(recorded(&log), ["left", "duplicate", "duplicate", "right"]);

        clear(&log);
        drop(first);
        hooks
            .snapshot(None)
            .run(&pre_context())
            .await
            .expect("one duplicate remains");
        assert_eq!(recorded(&log), ["left", "duplicate", "right"]);

        assert!(last.unregister());
        drop(left);
        drop(right);
        clear(&log);
        hooks
            .snapshot(None)
            .run(&pre_context())
            .await
            .expect("all live hooks removed");
        assert!(recorded(&log).is_empty());
    }

    #[tokio::test]
    async fn shared_pre_hook_runs_once_until_the_last_holder_is_dropped() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let hooks = PreExecutionHooks::new();
        let hook: Arc<dyn PreExecutionHook> = Arc::new(Records {
            label: "shared",
            log: Arc::clone(&log),
        });

        let first = hooks
            .register_live_shared("workspace".to_string(), None, Arc::clone(&hook))
            .expect("first holder");
        let second = hooks
            .register_live_shared("workspace".to_string(), None, Arc::clone(&hook))
            .expect("second holder");
        let cloned = first.clone();

        hooks
            .snapshot(None)
            .run(&pre_context())
            .await
            .expect("one shared hook");
        assert_eq!(recorded(&log), ["shared"]);

        drop(second);
        drop(first);
        clear(&log);
        hooks
            .snapshot(None)
            .run(&pre_context())
            .await
            .expect("cloned holder keeps hook live");
        assert_eq!(recorded(&log), ["shared"]);

        drop(cloned);
        assert!(hooks.is_empty());
    }

    #[tokio::test]
    async fn concurrent_shared_registrations_publish_one_chain_entry() {
        const HOLDERS: usize = 8;

        let log = Arc::new(Mutex::new(Vec::new()));
        let hooks = PreExecutionHooks::new();
        let hook: Arc<dyn PreExecutionHook> = Arc::new(Records {
            label: "shared",
            log: Arc::clone(&log),
        });
        let barrier = Arc::new(Barrier::new(HOLDERS));
        let registrars = (0..HOLDERS)
            .map(|_| {
                let hooks = hooks.clone();
                let hook = Arc::clone(&hook);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    hooks
                        .register_live_shared("workspace".to_string(), None, hook)
                        .expect("concurrent holder")
                })
            })
            .collect::<Vec<_>>();
        let holders = registrars
            .into_iter()
            .map(|registrar| registrar.join().expect("registrar exits"))
            .collect::<Vec<_>>();

        hooks
            .snapshot(None)
            .run(&pre_context())
            .await
            .expect("one shared hook");
        assert_eq!(recorded(&log), ["shared"]);

        drop(holders);
        assert!(hooks.is_empty());
    }

    #[tokio::test]
    async fn shared_hook_key_rejects_a_different_hook_or_audience() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let hooks = PreExecutionHooks::new();
        let hook: Arc<dyn PreExecutionHook> = Arc::new(Records {
            label: "original",
            log: Arc::clone(&log),
        });
        let guard = hooks
            .register_live_shared("workspace".to_string(), None, Arc::clone(&hook))
            .expect("original registration");

        let other_hook: Arc<dyn PreExecutionHook> = Arc::new(Records {
            label: "conflict",
            log: Arc::clone(&log),
        });
        let hook_conflict = hooks
            .register_live_shared("workspace".to_string(), None, other_hook)
            .expect_err("same key with another allocation must conflict");
        assert_eq!(hook_conflict.key(), "workspace");

        let audience_conflict = hooks
            .register_live_shared(
                "workspace".to_string(),
                Some(ToolAudience::new("alpha")),
                hook,
            )
            .expect_err("same key with another audience must conflict");
        assert_eq!(audience_conflict.key(), "workspace");

        hooks
            .snapshot(None)
            .run(&pre_context())
            .await
            .expect("original remains registered");
        assert_eq!(recorded(&log), ["original"]);
        drop(guard);
        assert!(hooks.is_empty());
    }

    #[tokio::test]
    async fn pre_and_post_shared_key_namespaces_are_independent() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let pre = PreExecutionHooks::new();
        let post = PostExecutionHooks::new();
        let pre_hook: Arc<dyn PreExecutionHook> = Arc::new(Records {
            label: "pre",
            log: Arc::clone(&log),
        });
        let post_hook: Arc<dyn PostExecutionHook> = Arc::new(Records {
            label: "post",
            log: Arc::clone(&log),
        });

        let _pre_holder = pre
            .register_live_shared("workspace".to_string(), None, pre_hook)
            .expect("pre key");
        let _post_holder = post
            .register_live_shared("workspace".to_string(), None, post_hook)
            .expect("same key is independent in post chain");

        pre.snapshot(None)
            .run(&pre_context())
            .await
            .expect("pre hook");
        post.snapshot(None)
            .run(&post_context())
            .await
            .expect("post hook");
        assert_eq!(recorded(&log), ["pre", "post"]);
    }

    struct BlockingPostHook {
        entered: Arc<Notify>,
        release: Arc<Notify>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl PostExecutionHook for BlockingPostHook {
        async fn post_tool_execution(
            &self,
            _context: &PostExecutionContext,
        ) -> Result<ResultDecision, RuntimeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            self.release.notified().await;
            Ok(ResultDecision::Keep)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_an_in_flight_registration_finishes_that_snapshot_only() {
        let hooks = PostExecutionHooks::new();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let guard = hooks.register_live(
            None,
            BlockingPostHook {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
                calls: Arc::clone(&calls),
            },
        );
        let snapshot = hooks.snapshot(None);
        let running = tokio::spawn(async move { snapshot.run(&post_context()).await });

        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .expect("blocking hook enters");
        let (dropped_tx, dropped_rx) = mpsc::channel();
        let dropper = thread::spawn(move || {
            drop(guard);
            let _ = dropped_tx.send(());
        });
        let dropped_before_release = dropped_rx.recv_timeout(Duration::from_millis(200)).is_ok();
        // Always release and join before asserting, so a lock-across-await
        // regression reports a bounded failure instead of hanging the suite.
        release.notify_one();
        let running_result = tokio::time::timeout(Duration::from_secs(2), running)
            .await
            .expect("hook task finishes")
            .expect("hook task joins")
            .expect("hook runs");
        if !dropped_before_release {
            let _ = dropped_rx.recv_timeout(Duration::from_secs(2));
        }
        let drop_result = dropper.join();
        assert!(hooks.snapshot(None).is_empty());
        assert_eq!(running_result, ResultDecision::Keep);
        assert!(
            dropped_before_release,
            "guard Drop must not wait for an in-flight hook callback"
        );
        drop_result.expect("guard dropper exits");
        hooks
            .snapshot(None)
            .run(&post_context())
            .await
            .expect("later empty snapshot runs");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    struct ReentrantDrop {
        hooks: PreExecutionHooks,
        dropped: Arc<AtomicBool>,
    }

    #[async_trait]
    impl PreExecutionHook for ReentrantDrop {
        async fn pre_tool_execution(
            &self,
            _context: &PreExecutionContext,
        ) -> Result<HookDecision, RuntimeError> {
            Ok(HookDecision::Allow)
        }
    }

    impl Drop for ReentrantDrop {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
            let transient = self.hooks.register_live(None, Allows);
            drop(transient);
        }
    }

    #[test]
    fn hook_captures_are_destroyed_after_the_registry_unlocks() {
        let hooks = PreExecutionHooks::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = hooks.register_live(
            None,
            ReentrantDrop {
                hooks: hooks.clone(),
                dropped: Arc::clone(&dropped),
            },
        );
        let (done_tx, done_rx) = mpsc::channel();
        let dropper = thread::spawn(move || {
            drop(guard);
            done_tx.send(()).expect("report guard drop");
        });

        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("reentrant hook Drop must not deadlock");
        dropper.join().expect("dropper exits");
        assert!(dropped.load(Ordering::SeqCst));
        assert!(hooks.is_empty());
    }

    #[test]
    fn rejected_shared_capture_is_destroyed_after_the_registry_unlocks() {
        let hooks = PreExecutionHooks::new();
        let original: Arc<dyn PreExecutionHook> = Arc::new(Allows);
        let guard = hooks
            .register_live_shared("workspace".to_string(), None, original)
            .expect("original shared hook");
        let dropped = Arc::new(AtomicBool::new(false));
        let conflicting: Arc<dyn PreExecutionHook> = Arc::new(ReentrantDrop {
            hooks: hooks.clone(),
            dropped: Arc::clone(&dropped),
        });
        let (done_tx, done_rx) = mpsc::channel();
        let registration_hooks = hooks.clone();
        let registrar = thread::spawn(move || {
            let conflict = registration_hooks
                .register_live_shared("workspace".to_string(), None, conflicting)
                .expect_err("different allocation conflicts");
            done_tx.send(conflict).expect("report conflict");
        });

        let conflict = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("rejected capture Drop must not deadlock");
        registrar.join().expect("registrar exits");
        assert_eq!(conflict.key(), "workspace");
        assert!(dropped.load(Ordering::SeqCst));
        drop(guard);
        assert!(hooks.is_empty());
    }

    #[test]
    fn stale_shared_key_is_replaced_and_cannot_remove_the_new_generation() {
        let registry: Arc<RwLock<LiveHookRegistry<Arc<dyn PreExecutionHook>>>> =
            Arc::new(RwLock::new(LiveHookRegistry::new()));
        let stale_registration = {
            let registration = Arc::new(SharedLiveHookRegistration {
                registry: Weak::new(),
                id: 41,
                key: "workspace".to_string(),
                audience: None,
            });
            Arc::downgrade(&registration)
        };
        registry
            .write()
            .expect("registry")
            .entries
            .push(LiveHookEntry {
                id: 41,
                audience: None,
                value: Arc::new(Allows),
                shared: Some(SharedHookEntry {
                    key: "workspace".to_string(),
                    registration: stale_registration,
                }),
            });

        let fresh_hook: Arc<dyn PreExecutionHook> = Arc::new(Allows);
        let fresh = LiveHookRegistry::register_shared(
            &registry,
            42,
            "workspace".to_string(),
            None,
            Arc::clone(&fresh_hook),
            Arc::ptr_eq,
        )
        .expect("dead holder metadata is replaced");
        {
            let registry = registry.read().expect("registry");
            assert_eq!(registry.entries.len(), 1);
            assert_eq!(registry.entries[0].id, 42);
            assert!(Arc::ptr_eq(&registry.entries[0].value, &fresh_hook));
        }

        let stale_generation = Arc::new(SharedLiveHookRegistration {
            registry: Arc::downgrade(&registry),
            id: 41,
            key: "workspace".to_string(),
            audience: None,
        });
        drop(stale_generation);
        assert_eq!(registry.read().expect("registry").entries.len(), 1);

        drop(fresh);
        assert!(registry.read().expect("registry").is_empty());
    }

    #[test]
    fn registration_drop_recovers_a_poisoned_registry() {
        let hooks = PreExecutionHooks::new();
        let guard = hooks.register_live(None, Allows);
        let registry = guard.inner.registry.upgrade().expect("live hook registry");
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _locked = registry.write().expect("initially healthy registry");
            panic!("poison live hook registry");
        }));

        assert!(
            std::panic::catch_unwind(AssertUnwindSafe(|| drop(guard))).is_ok(),
            "registration Drop must recover the poisoned registry"
        );
        assert!(hooks.is_empty());
    }
}
