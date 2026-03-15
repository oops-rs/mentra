use std::{path::PathBuf, sync::Arc};

use serde::{Deserialize, Serialize};

use crate::{
    provider::ProviderError,
    runtime::{AuditStore, RuleMatch, error::RuntimeError},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuntimeHookEvent {
    AuthorizationDenied {
        agent_id: String,
        action: String,
        detail: String,
    },
    ShellApprovalRequired {
        agent_id: String,
        tool_name: String,
        command: String,
        cwd: PathBuf,
        parsed_kind: String,
        matched_rules: Vec<RuleMatch>,
        justification: Option<String>,
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
}

impl RuntimeHookEvent {
    fn scope(&self) -> String {
        match self {
            Self::AuthorizationDenied { agent_id, .. } => agent_id.clone(),
            Self::ShellApprovalRequired { agent_id, .. } => agent_id.clone(),
            Self::RecoveryPrepared {
                runtime_instance_id,
            } => runtime_instance_id.clone(),
            Self::ModelRequestStarted { agent_id, .. }
            | Self::ModelRequestFinished { agent_id, .. }
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
            | Self::RunAborted { agent_id, .. } => agent_id.clone(),
        }
    }

    fn event_type(&self) -> &'static str {
        match self {
            Self::AuthorizationDenied { .. } => "authorization_denied",
            Self::ShellApprovalRequired { .. } => "shell_approval_required",
            Self::RecoveryPrepared { .. } => "recovery_prepared",
            Self::ModelRequestStarted { .. } => "model_request_started",
            Self::ModelRequestFinished { .. } => "model_request_finished",
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

/// Returns whether a provider error is likely transient and worth retrying.
pub fn is_transient_provider_error(error: &ProviderError) -> bool {
    match error {
        ProviderError::Transport(_) | ProviderError::Decode(_) => true,
        ProviderError::Http { status, .. } => {
            status.is_server_error()
                || *status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || *status == reqwest::StatusCode::REQUEST_TIMEOUT
        }
        ProviderError::Serialize(_)
        | ProviderError::Deserialize(_)
        | ProviderError::InvalidRequest(_)
        | ProviderError::InvalidResponse(_)
        | ProviderError::MalformedStream(_) => false,
    }
}

/// Returns whether a runtime error is backed by a transient provider failure.
pub fn is_transient_runtime_error(error: &RuntimeError) -> bool {
    match error {
        RuntimeError::FailedToSendRequest(source)
        | RuntimeError::FailedToListModels(source)
        | RuntimeError::FailedToStreamResponse(source)
        | RuntimeError::FailedToCompactHistory(source) => is_transient_provider_error(source),
        _ => false,
    }
}
