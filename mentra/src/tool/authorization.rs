use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    runtime::RuntimeError,
    tool::{
        ToolApprovalCategory, ToolCapability, ToolDurability, ToolExecutionCategory,
        ToolSideEffectLevel,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolAuthorizationOutcome {
    Allow,
    Prompt,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolAuthorizationPreview {
    pub working_directory: PathBuf,
    pub capabilities: Vec<ToolCapability>,
    pub side_effect_level: ToolSideEffectLevel,
    pub durability: ToolDurability,
    pub execution_category: ToolExecutionCategory,
    pub approval_category: ToolApprovalCategory,
    pub raw_input: Value,
    pub structured_input: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolAuthorizationDecision {
    pub outcome: ToolAuthorizationOutcome,
    pub reason: Option<String>,
}

impl ToolAuthorizationDecision {
    pub fn allow() -> Self {
        Self {
            outcome: ToolAuthorizationOutcome::Allow,
            reason: None,
        }
    }

    pub fn prompt(reason: impl Into<String>) -> Self {
        Self {
            outcome: ToolAuthorizationOutcome::Prompt,
            reason: Some(reason.into()),
        }
    }

    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            outcome: ToolAuthorizationOutcome::Deny,
            reason: Some(reason.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolAuthorizationRequest {
    pub agent_id: String,
    pub agent_name: String,
    pub model: String,
    pub history_len: usize,
    pub tool_call_id: String,
    pub tool_name: String,
    pub preview: ToolAuthorizationPreview,
}

#[async_trait]
pub trait ToolAuthorizer: Send + Sync {
    async fn authorize(
        &self,
        request: &ToolAuthorizationRequest,
    ) -> Result<ToolAuthorizationDecision, RuntimeError>;

    fn timeout(&self) -> Option<Duration> {
        None
    }
}
