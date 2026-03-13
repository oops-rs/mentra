use std::{path::PathBuf, sync::Arc};

use serde::{Deserialize, Serialize};

use crate::{
    provider::ProviderError,
    runtime::{RuleMatch, error::RuntimeError, store::RuntimeStore},
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
            Self::RunAborted { .. } => "run_aborted",
        }
    }
}

pub trait RuntimeHook: Send + Sync {
    fn on_event(
        &self,
        store: &dyn RuntimeStore,
        event: &RuntimeHookEvent,
    ) -> Result<(), RuntimeError>;
}

pub struct AuditHook;
pub type AuditLogHook = AuditHook;

impl RuntimeHook for AuditHook {
    fn on_event(
        &self,
        store: &dyn RuntimeStore,
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
        store: &dyn RuntimeStore,
        event: &RuntimeHookEvent,
    ) -> Result<(), RuntimeError> {
        for hook in &self.hooks {
            hook.on_event(store, event)?;
        }
        Ok(())
    }
}

pub(crate) fn is_transient_provider_error(error: &ProviderError) -> bool {
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
