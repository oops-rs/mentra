use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    provider::{ProviderError, TokenUsage},
    runtime::{AuditStore, RuntimeStore, error::RuntimeError},
    tool::{ToolAuthorizationOutcome, ToolAuthorizationPreview, ToolResultContent},
};

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

#[derive(Clone, Default)]
pub struct PreExecutionHooks {
    hooks: Vec<Arc<dyn PreExecutionHook>>,
}

impl PreExecutionHooks {
    pub fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    pub fn with_hook<H>(mut self, hook: H) -> Self
    where
        H: PreExecutionHook + 'static,
    {
        self.hooks.push(Arc::new(hook));
        self
    }

    /// Runs every hook in order, threading any modification through the rest.
    ///
    /// Returns the surviving decision: a `Deny` from any hook short-circuits,
    /// and otherwise the last `Modify` (if any) is what the tool should run
    /// with. Each hook sees the input as its predecessors left it, so
    /// modifications compose and no hook can route a call around a later one.
    pub async fn run(&self, context: &PreExecutionContext) -> Result<HookDecision, RuntimeError> {
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

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
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

#[derive(Clone, Default)]
pub struct PostExecutionHooks {
    hooks: Vec<Arc<dyn PostExecutionHook>>,
}

impl PostExecutionHooks {
    pub fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    pub fn with_hook<H>(mut self, hook: H) -> Self
    where
        H: PostExecutionHook + 'static,
    {
        self.hooks.push(Arc::new(hook));
        self
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
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
        if self.hooks.is_empty() {
            return Ok(ResultDecision::Keep);
        }

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
