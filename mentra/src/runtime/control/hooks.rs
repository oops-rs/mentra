use std::{path::PathBuf, sync::Arc};

use serde::{Deserialize, Serialize};

use crate::{
    provider::{ProviderError, TokenUsage},
    runtime::{AuditStore, error::RuntimeError},
    tool::{ToolAuthorizationOutcome, ToolAuthorizationPreview},
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDecision {
    Allow,
    Deny(String),
}

pub trait PreExecutionHook: Send + Sync {
    fn pre_tool_execution(
        &self,
        context: &PreExecutionContext,
    ) -> Result<HookDecision, RuntimeError>;
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

    pub fn run(&self, context: &PreExecutionContext) -> Result<HookDecision, RuntimeError> {
        for hook in &self.hooks {
            match hook.pre_tool_execution(context)? {
                HookDecision::Allow => continue,
                deny @ HookDecision::Deny(_) => return Ok(deny),
            }
        }
        Ok(HookDecision::Allow)
    }

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
        ProviderError::Serialize(_)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_context(tool_name: &str) -> PreExecutionContext {
        PreExecutionContext {
            agent_id: "agent-1".to_string(),
            tool_name: tool_name.to_string(),
            tool_call_id: "call-1".to_string(),
            input_json: "{}".to_string(),
        }
    }

    struct AllowHook;
    impl PreExecutionHook for AllowHook {
        fn pre_tool_execution(
            &self,
            _context: &PreExecutionContext,
        ) -> Result<HookDecision, RuntimeError> {
            Ok(HookDecision::Allow)
        }
    }

    struct DenyHook;
    impl PreExecutionHook for DenyHook {
        fn pre_tool_execution(
            &self,
            _context: &PreExecutionContext,
        ) -> Result<HookDecision, RuntimeError> {
            Ok(HookDecision::Deny("denied by DenyHook".to_string()))
        }
    }

    struct ToolNameDenyHook {
        blocked_tool: String,
    }
    impl PreExecutionHook for ToolNameDenyHook {
        fn pre_tool_execution(
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

    #[test]
    fn empty_pre_hooks_allows() {
        let hooks = PreExecutionHooks::new();
        let result = hooks.run(&make_context("shell")).unwrap();
        assert_eq!(result, HookDecision::Allow);
    }

    #[test]
    fn all_allow_hooks_allows() {
        let hooks = PreExecutionHooks::new()
            .with_hook(AllowHook)
            .with_hook(AllowHook);
        let result = hooks.run(&make_context("files")).unwrap();
        assert_eq!(result, HookDecision::Allow);
    }

    #[test]
    fn first_deny_wins() {
        let hooks = PreExecutionHooks::new()
            .with_hook(AllowHook)
            .with_hook(DenyHook)
            .with_hook(AllowHook);
        let result = hooks.run(&make_context("any_tool")).unwrap();
        assert_eq!(result, HookDecision::Deny("denied by DenyHook".to_string()));
    }

    #[test]
    fn conditional_deny_by_tool_name() {
        let hooks = PreExecutionHooks::new().with_hook(ToolNameDenyHook {
            blocked_tool: "shell".to_string(),
        });

        let shell_result = hooks.run(&make_context("shell")).unwrap();
        assert_eq!(
            shell_result,
            HookDecision::Deny("tool 'shell' is blocked".to_string())
        );

        let files_result = hooks.run(&make_context("files")).unwrap();
        assert_eq!(files_result, HookDecision::Allow);
    }
}
