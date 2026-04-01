use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, oneshot};

use super::event::{PermissionRuleScope, SessionEvent};
use crate::{
    runtime::RuntimeError,
    tool::{
        ToolAuthorizationDecision, ToolAuthorizationOutcome, ToolAuthorizationRequest,
        ToolAuthorizer,
    },
};

/// A pending permission request awaiting a UI decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub request_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub description: String,
    /// JSON-encoded preview data. Stored as `String` because
    /// `serde_json::Value` does not implement `Eq`.
    pub preview: String,
}

/// The response to a permission request from the UI layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionDecision {
    pub allow: bool,
    pub remember_as: Option<PermissionRuleScope>,
}

impl PermissionDecision {
    /// Allow the tool call without remembering.
    pub fn allow() -> Self {
        Self {
            allow: true,
            remember_as: None,
        }
    }

    /// Deny the tool call without remembering.
    pub fn deny() -> Self {
        Self {
            allow: false,
            remember_as: None,
        }
    }

    /// Allow the tool call and remember the decision for the given scope.
    pub fn allow_and_remember(scope: PermissionRuleScope) -> Self {
        Self {
            allow: true,
            remember_as: Some(scope),
        }
    }

    /// Deny the tool call and remember the decision for the given scope.
    pub fn deny_and_remember(scope: PermissionRuleScope) -> Self {
        Self {
            allow: false,
            remember_as: Some(scope),
        }
    }
}

/// Key for looking up remembered permission rules.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RuleKey {
    pub tool_name: String,
    pub pattern: Option<String>,
}

/// A stored permission rule that was previously decided by the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RememberedRule {
    pub key: RuleKey,
    pub allow: bool,
    pub scope: PermissionRuleScope,
}

/// Thread-safe in-memory store for remembered permission rules.
#[derive(Debug, Clone)]
pub struct RuleStore {
    inner: Arc<Mutex<HashMap<RuleKey, RememberedRule>>>,
}

impl Default for RuleStore {
    fn default() -> Self {
        Self::new()
    }
}

impl RuleStore {
    /// Creates an empty rule store.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Adds or overwrites a remembered rule.
    pub fn add_rule(&self, rule: RememberedRule) {
        let mut rules = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        rules.insert(rule.key.clone(), rule);
    }

    /// Checks whether a tool is allowed by a remembered rule.
    ///
    /// Returns `Some(true)` if allowed, `Some(false)` if denied, or `None` if
    /// no matching rule exists.
    pub fn check(&self, tool_name: &str) -> Option<bool> {
        let rules = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let key = RuleKey {
            tool_name: tool_name.to_owned(),
            pattern: None,
        };
        rules.get(&key).map(|rule| rule.allow)
    }

    /// Returns all remembered rules as a vector.
    pub fn rules(&self) -> Vec<RememberedRule> {
        let rules = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        rules.values().cloned().collect()
    }

    /// Removes all rules that match the given scope.
    pub fn clear_scope(&self, scope: PermissionRuleScope) {
        let mut rules = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        rules.retain(|_, rule| rule.scope != scope);
    }
}

/// Thread-safe store for pending permission requests that can be resolved later.
#[derive(Debug, Clone, Default)]
pub(crate) struct PendingPermissionStore {
    inner: Arc<Mutex<HashMap<String, PendingPermissionEntry>>>,
}

impl PendingPermissionStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn insert(&self, request_id: String, entry: PendingPermissionEntry) {
        let mut pending = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        pending.insert(request_id, entry);
    }

    pub(crate) fn remove(&self, request_id: &str) -> Option<PendingPermissionEntry> {
        let mut pending = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        pending.remove(request_id)
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, request_id: &str) -> bool {
        let pending = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        pending.contains_key(request_id)
    }
}

/// Internal entry tracking a pending permission with its oneshot response channel.
#[derive(Debug)]
pub(crate) struct PendingPermissionEntry {
    pub(crate) tool_call_id: String,
    pub(crate) tool_name: String,
    pub(crate) sender: oneshot::Sender<PermissionDecision>,
}

/// Session-scoped wrapper around the runtime tool authorizer.
///
/// This is the bridge that turns `Prompt` outcomes into typed
/// `SessionEvent::PermissionRequested` events, stores the pending request, and
/// suspends execution until a matching decision arrives.
#[derive(Clone)]
pub(crate) struct SessionToolAuthorizer {
    inner: Option<Arc<dyn ToolAuthorizer>>,
    event_tx: broadcast::Sender<SessionEvent>,
    pending_permissions: PendingPermissionStore,
    rule_store: RuleStore,
}

impl SessionToolAuthorizer {
    pub(crate) fn new(
        inner: Option<Arc<dyn ToolAuthorizer>>,
        event_tx: broadcast::Sender<SessionEvent>,
        pending_permissions: PendingPermissionStore,
        rule_store: RuleStore,
    ) -> Self {
        Self {
            inner,
            event_tx,
            pending_permissions,
            rule_store,
        }
    }
}

#[async_trait]
impl ToolAuthorizer for SessionToolAuthorizer {
    async fn authorize(
        &self,
        request: &ToolAuthorizationRequest,
    ) -> Result<ToolAuthorizationDecision, RuntimeError> {
        if let Some(allow) = self.rule_store.check(&request.tool_name) {
            return Ok(if allow {
                ToolAuthorizationDecision::allow()
            } else {
                ToolAuthorizationDecision::deny("blocked by remembered session rule")
            });
        }

        let Some(inner) = &self.inner else {
            return Ok(ToolAuthorizationDecision::allow());
        };

        let decision = inner.authorize(request).await?;
        if decision.outcome != ToolAuthorizationOutcome::Prompt {
            return Ok(decision);
        }

        let request_id = format!("perm-{}", request.tool_call_id);
        let description = decision
            .reason
            .clone()
            .unwrap_or_else(|| format!("Approval required for {}", request.tool_name));
        let preview = serde_json::to_string(&request.preview.structured_input)
            .unwrap_or_else(|_| "{}".to_string());
        let (sender, receiver) = oneshot::channel();

        self.pending_permissions.insert(
            request_id.clone(),
            PendingPermissionEntry {
                tool_call_id: request.tool_call_id.clone(),
                tool_name: request.tool_name.clone(),
                sender,
            },
        );

        let _ = self.event_tx.send(SessionEvent::PermissionRequested {
            request_id: request_id.clone(),
            tool_call_id: request.tool_call_id.clone(),
            tool_name: request.tool_name.clone(),
            description,
            preview,
        });

        let resolved = receiver
            .await
            .unwrap_or_else(|_| PermissionDecision::deny());
        Ok(if resolved.allow {
            ToolAuthorizationDecision::allow()
        } else {
            ToolAuthorizationDecision::deny("denied by session approver")
        })
    }

    fn timeout(&self) -> Option<Duration> {
        self.inner
            .as_ref()
            .and_then(|authorizer| authorizer.timeout())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::tool::{
        ToolApprovalCategory, ToolAuthorizationPreview, ToolCapability, ToolDurability,
        ToolExecutionCategory, ToolSideEffectLevel,
    };

    #[derive(Clone)]
    struct PromptAuthorizer;

    #[async_trait]
    impl ToolAuthorizer for PromptAuthorizer {
        async fn authorize(
            &self,
            _request: &ToolAuthorizationRequest,
        ) -> Result<ToolAuthorizationDecision, RuntimeError> {
            Ok(ToolAuthorizationDecision::prompt("needs manual review"))
        }
    }

    fn sample_request() -> ToolAuthorizationRequest {
        ToolAuthorizationRequest {
            agent_id: "agent-1".to_string(),
            agent_name: "agent".to_string(),
            model: "mock-model".to_string(),
            history_len: 3,
            tool_call_id: "tool-1".to_string(),
            tool_name: "shell".to_string(),
            preview: ToolAuthorizationPreview {
                working_directory: std::env::temp_dir(),
                capabilities: vec![ToolCapability::ProcessExec],
                side_effect_level: ToolSideEffectLevel::Process,
                durability: ToolDurability::Ephemeral,
                execution_category: ToolExecutionCategory::ExclusiveLocalMutation,
                approval_category: ToolApprovalCategory::Process,
                raw_input: json!({ "command": "cargo test" }),
                structured_input: json!({ "kind": "shell", "command": "cargo test" }),
            },
        }
    }

    #[tokio::test]
    async fn session_tool_authorizer_emits_permission_request_and_waits() {
        let (event_tx, mut rx) = broadcast::channel(8);
        let pending = PendingPermissionStore::new();
        let authorizer = SessionToolAuthorizer::new(
            Some(Arc::new(PromptAuthorizer)),
            event_tx,
            pending.clone(),
            RuleStore::new(),
        );
        let request = sample_request();

        let authorize_task = tokio::spawn({
            let authorizer = authorizer.clone();
            let request = request.clone();
            async move { authorizer.authorize(&request).await.unwrap() }
        });

        let event = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("permission request should arrive")
            .expect("event should be present");

        let request_id = match event {
            SessionEvent::PermissionRequested {
                request_id,
                tool_call_id,
                tool_name,
                ..
            } => {
                assert_eq!(tool_call_id, "tool-1");
                assert_eq!(tool_name, "shell");
                request_id
            }
            other => panic!("expected PermissionRequested, got {other:?}"),
        };

        assert!(pending.contains(&request_id));
        let entry = pending
            .remove(&request_id)
            .expect("pending permission should be registered");
        entry
            .sender
            .send(PermissionDecision::allow())
            .expect("decision send should succeed");

        let decision = tokio::time::timeout(Duration::from_millis(200), authorize_task)
            .await
            .expect("authorization should resume")
            .expect("task should succeed");
        assert_eq!(decision.outcome, ToolAuthorizationOutcome::Allow);
    }
}
