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

impl ToolAuthorizationPreview {
    /// What this call was classified as, without its input.
    ///
    /// The classification is the part a policy reasons about, and it is the
    /// part that survives past the authorizer — see [`ToolClassification`].
    pub fn classification(&self) -> ToolClassification {
        self.into()
    }
}

/// What the runtime worked out about a tool call before anyone was asked to
/// approve it: what it can touch, how far its effects reach, whether it can be
/// replayed, which scheduler lane it will run in, and which approval group it
/// falls under.
///
/// [`execution_category`](Self::execution_category) is the lane the scheduler
/// will actually use, asked of the tool with this call's input and carrying
/// the coercion a terminal tool gets -- not the category the tool's descriptor
/// declares. The two differ: `files` declares an exclusive mutation and
/// reports a parallel read for a batch that only reads.
///
/// This is [`ToolAuthorizationPreview`] with the call's input left off. The
/// input already travels beside it wherever this type is carried, and the
/// classification alone is what a policy matches on: "allow edits, refuse the
/// network" is [`ToolSideEffectLevel::LocalState`] against
/// [`ToolSideEffectLevel::External`], a distinction that no amount of reading
/// tool names and parsing arguments recovers reliably.
///
/// Deliberately not [`Default`]: the two fields a policy leans on hardest
/// zero out permissively — [`capabilities`](Self::capabilities) to empty and
/// [`side_effect_level`](Self::side_effect_level) to
/// [`ToolSideEffectLevel::None`] — so a classification conjured from nothing
/// would read as a call that touches nothing, the one answer a policy must
/// never be handed by accident. Where a classification may be absent, say so
/// with an [`Option`].
///
/// # Wire format
///
/// The enums inside serialize in their derived form — PascalCase, externally
/// tagged: `"ReplaySafe"`, `"External"`, `{"Custom":"network"}` — even though
/// this type rides inside
/// [`SessionEvent`](crate::session::SessionEvent), whose own tags and fields
/// are snake_case. The mismatch is kept deliberately. The same five enums are
/// shared with [`ToolAuthorizationPreview`], which has serialized this way
/// since it shipped and is written verbatim into audit rows by the audit
/// hook, so renaming their representation would change a released wire format
/// and orphan recorded data — while reserializing them differently for this
/// one field would have the same value print two ways in one API. One
/// representation, consistent with itself, is the smaller surprise; a host
/// parsing the event stream as JSON should expect PascalCase inside this
/// field alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolClassification {
    pub capabilities: Vec<ToolCapability>,
    pub side_effect_level: ToolSideEffectLevel,
    pub durability: ToolDurability,
    pub execution_category: ToolExecutionCategory,
    pub approval_category: ToolApprovalCategory,
}

impl From<&ToolAuthorizationPreview> for ToolClassification {
    fn from(preview: &ToolAuthorizationPreview) -> Self {
        Self {
            capabilities: preview.capabilities.clone(),
            side_effect_level: preview.side_effect_level,
            durability: preview.durability,
            execution_category: preview.execution_category,
            approval_category: preview.approval_category,
        }
    }
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

/// Consulted after the pre-execution hooks and the schema check, and able to
/// stop a call — but not to rewrite it.
///
/// The request's [`structured_input`](ToolAuthorizationPreview::structured_input)
/// is the input the tool will execute with: every
/// [`PreExecutionHook`](crate::runtime::control::PreExecutionHook) has already
/// run, any [`HookDecision::Modify`](crate::runtime::control::HookDecision::Modify)
/// has been applied and validated against the tool's `input_schema`, and
/// nothing rewrites the call between this answer and execution. A host that
/// remembers a decision as a rule keyed on that input can therefore replay it
/// against future calls and know the rule describes what ran.
///
/// The authorizer is not consulted for a call a hook denied, nor for one whose
/// input — the model's or a hook's — does not fit the schema.
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

/// Forwards to the authorizer inside.
///
/// Lets a caller hold an authorizer it chose at runtime — one of several, or
/// none — and still hand it to anything taking `impl ToolAuthorizer`, without
/// each caller writing this impl itself.
#[async_trait]
impl<T: ToolAuthorizer + ?Sized> ToolAuthorizer for Box<T> {
    async fn authorize(
        &self,
        request: &ToolAuthorizationRequest,
    ) -> Result<ToolAuthorizationDecision, RuntimeError> {
        (**self).authorize(request).await
    }

    fn timeout(&self) -> Option<Duration> {
        (**self).timeout()
    }
}

/// Forwards to the authorizer inside, for a shared one.
#[async_trait]
impl<T: ToolAuthorizer + ?Sized> ToolAuthorizer for std::sync::Arc<T> {
    async fn authorize(
        &self,
        request: &ToolAuthorizationRequest,
    ) -> Result<ToolAuthorizationDecision, RuntimeError> {
        (**self).authorize(request).await
    }

    fn timeout(&self) -> Option<Duration> {
        (**self).timeout()
    }
}
