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

/// What a refusal says when the deciding layer offered no reason of its own.
const DENIED_BY_SESSION_APPROVER: &str = "denied by session approver";

/// What a remembered refusal says when the rule it was stored as kept no reason.
const BLOCKED_BY_REMEMBERED_RULE: &str = "blocked by remembered session rule";

/// What the model reads when a remembered rule refuses a call.
///
/// The words the host first refused with come back in front, because they are
/// the part that says what to do instead; the rest says the answer is standing,
/// because a model told only that something was blocked asks again, and asking
/// again is the one thing that cannot change a remembered rule.
fn remembered_denial(reason: Option<&str>) -> String {
    match reason {
        Some(reason) => format!(
            "{reason} — remembered from an earlier refusal, so asking again will not change it"
        ),
        None => BLOCKED_BY_REMEMBERED_RULE.to_string(),
    }
}

/// The response to a permission request from the UI layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionDecision {
    pub allow: bool,
    pub remember_as: Option<PermissionRuleScope>,
    /// Why the call was refused, in the words the model will read.
    ///
    /// A denial reaches the model as the tool's result, so what it says
    /// changes what the model does next: told only that something was denied
    /// it tries the write again, told that this run does not allow writes it
    /// stops and reports. Set it with [`PermissionDecision::with_reason`].
    /// Ignored when `allow` is set, and a refusal that leaves it unset still
    /// reads "denied by session approver" as it always has.
    pub reason: Option<String>,
}

impl PermissionDecision {
    /// Allow the tool call without remembering.
    pub fn allow() -> Self {
        Self {
            allow: true,
            remember_as: None,
            reason: None,
        }
    }

    /// Deny the tool call without remembering.
    pub fn deny() -> Self {
        Self {
            allow: false,
            remember_as: None,
            reason: None,
        }
    }

    /// Allow the tool call and remember the decision for the given scope.
    pub fn allow_and_remember(scope: PermissionRuleScope) -> Self {
        Self {
            allow: true,
            remember_as: Some(scope),
            reason: None,
        }
    }

    /// Deny the tool call and remember the decision for the given scope.
    pub fn deny_and_remember(scope: PermissionRuleScope) -> Self {
        Self {
            allow: false,
            remember_as: Some(scope),
            reason: None,
        }
    }

    /// The same decision, carrying the reason the model should read.
    ///
    /// Only refusals have anything to explain: an allowed call explains
    /// itself by happening.
    pub fn with_reason(self, reason: impl Into<String>) -> Self {
        Self {
            reason: Some(reason.into()),
            ..self
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
    /// Why the remembered refusal refused, in the words the model will read.
    ///
    /// A remembered rule answers every later call itself, without ever
    /// reaching the approver again, so a rule that keeps the verdict and drops
    /// the reason lets the host explain itself exactly once: every repeat after
    /// that reads only that something was blocked. Written from
    /// [`PermissionDecision::reason`] when the remembered decision is a
    /// refusal, and left unset for an allow, which explains itself by
    /// happening. A refusal that kept no reason still reads "blocked by
    /// remembered session rule" as it always has.
    ///
    /// `serde(default)` keeps rules persisted before this field existed
    /// deserializing unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
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
    /// Pattern rules are matched against `input_json` using glob syntax and
    /// take precedence over bare (no-pattern) rules. Returns `Some(true)` if
    /// allowed, `Some(false)` if denied, or `None` if no matching rule exists.
    /// Use [`RuleStore::matching_rule`] when the rule's own reason matters.
    pub fn check(&self, tool_name: &str, input_json: Option<&str>) -> Option<bool> {
        self.matching_rule(tool_name, input_json)
            .map(|rule| rule.allow)
    }

    /// The remembered rule that answers a call, if one does.
    ///
    /// Matches exactly as [`RuleStore::check`] does — pattern rules against
    /// `input_json` by glob, taking precedence over bare (no-pattern) rules —
    /// and hands back the whole rule, so a refusal can restate the reason it
    /// was remembered with rather than only its verdict.
    pub fn matching_rule(
        &self,
        tool_name: &str,
        input_json: Option<&str>,
    ) -> Option<RememberedRule> {
        let rules = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        let mut pattern_match: Option<&RememberedRule> = None;
        let mut bare_match: Option<&RememberedRule> = None;

        for rule in rules.values() {
            if rule.key.tool_name != tool_name {
                continue;
            }
            match &rule.key.pattern {
                Some(glob) => {
                    if let Some(json) = input_json
                        && glob_match::glob_match(glob, json)
                    {
                        pattern_match = Some(rule);
                    }
                }
                None => {
                    bare_match = Some(rule);
                }
            }
        }

        pattern_match.or(bare_match).cloned()
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
        let input_json = serde_json::to_string(&request.preview.structured_input).ok();
        if let Some(rule) = self
            .rule_store
            .matching_rule(&request.tool_name, input_json.as_deref())
        {
            return Ok(if rule.allow {
                ToolAuthorizationDecision::allow()
            } else {
                // The approver is not consulted again, so the rule is the only
                // place the original reason can still come from.
                ToolAuthorizationDecision::deny(remembered_denial(rule.reason.as_deref()))
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
            // Whoever answered gets to say why, because that text is what the
            // model reads; a refusal that explains nothing keeps the wording
            // this has always used.
            ToolAuthorizationDecision::deny(
                resolved
                    .reason
                    .unwrap_or_else(|| DENIED_BY_SESSION_APPROVER.to_string()),
            )
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

    /// Refuses in words no remembered rule would use, so a call that reached
    /// the approver shows up as a wrong string rather than as a test that
    /// blocks forever waiting for an answer nobody will give.
    #[derive(Clone)]
    struct ApproverOfLastResort;

    #[async_trait]
    impl ToolAuthorizer for ApproverOfLastResort {
        async fn authorize(
            &self,
            _request: &ToolAuthorizationRequest,
        ) -> Result<ToolAuthorizationDecision, RuntimeError> {
            Ok(ToolAuthorizationDecision::deny(
                "the approver was asked again",
            ))
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

    /// Runs one authorize-and-resolve round trip, answering with `decision`,
    /// and returns what the authorizer handed back to the tool loop.
    async fn resolved_with(decision: PermissionDecision) -> ToolAuthorizationDecision {
        let (event_tx, mut rx) = broadcast::channel(8);
        let pending = PendingPermissionStore::new();
        let authorizer = SessionToolAuthorizer::new(
            Some(Arc::new(PromptAuthorizer)),
            event_tx,
            pending.clone(),
            RuleStore::new(),
        );

        let authorize_task = tokio::spawn({
            let authorizer = authorizer.clone();
            async move { authorizer.authorize(&sample_request()).await.unwrap() }
        });

        let event = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("permission request should arrive")
            .expect("event should be present");
        let SessionEvent::PermissionRequested { request_id, .. } = &event else {
            panic!("expected PermissionRequested, got {event:?}");
        };

        pending
            .remove(request_id)
            .expect("pending permission should be registered")
            .sender
            .send(decision)
            .expect("decision send should succeed");

        tokio::time::timeout(Duration::from_millis(200), authorize_task)
            .await
            .expect("authorization should resume")
            .expect("task should succeed")
    }

    #[tokio::test]
    async fn a_reasoned_denial_carries_its_words_to_the_tool_result() {
        // The reason becomes the tool result the model reads, so anything
        // rewritten or dropped on the way is a reason it never sees.
        let decision =
            resolved_with(PermissionDecision::deny().with_reason("this run does not allow writes"))
                .await;

        assert_eq!(decision.outcome, ToolAuthorizationOutcome::Deny);
        assert_eq!(
            decision.reason.as_deref(),
            Some("this run does not allow writes")
        );
    }

    #[tokio::test]
    async fn a_denial_with_nothing_to_say_keeps_the_standing_wording() {
        let decision = resolved_with(PermissionDecision::deny()).await;

        assert_eq!(decision.outcome, ToolAuthorizationOutcome::Deny);
        assert_eq!(decision.reason.as_deref(), Some(DENIED_BY_SESSION_APPROVER));
    }

    #[tokio::test]
    async fn a_reason_on_an_allowed_call_changes_nothing() {
        let decision = resolved_with(PermissionDecision::allow().with_reason("ignored")).await;

        assert_eq!(decision.outcome, ToolAuthorizationOutcome::Allow);
        assert_eq!(
            decision.reason, None,
            "an allowed call has nothing to explain"
        );
    }

    /// Answers one authorize call from `store` alone. The approver behind it
    /// refuses in its own words, so a rule that failed to answer is visible.
    async fn answered_by_rule(store: RuleStore) -> ToolAuthorizationDecision {
        let (event_tx, _rx) = broadcast::channel(8);
        let authorizer = SessionToolAuthorizer::new(
            Some(Arc::new(ApproverOfLastResort)),
            event_tx,
            PendingPermissionStore::new(),
            store,
        );

        authorizer
            .authorize(&sample_request())
            .await
            .expect("authorization should resolve")
    }

    /// A bare `shell` rule for the session, remembered with `reason` or without.
    fn shell_rule(allow: bool, reason: Option<&str>) -> RememberedRule {
        RememberedRule {
            key: RuleKey {
                tool_name: "shell".to_owned(),
                pattern: None,
            },
            allow,
            scope: PermissionRuleScope::Session,
            reason: reason.map(str::to_owned),
        }
    }

    #[tokio::test]
    async fn a_remembered_refusal_restates_the_reason_it_was_remembered_with() {
        // Nothing asks the approver a second time, so the rule is the only
        // thing left that knows why the first answer was no.
        let store = RuleStore::new();
        store.add_rule(shell_rule(false, Some("this run does not allow writes")));

        let decision = answered_by_rule(store).await;

        assert_eq!(decision.outcome, ToolAuthorizationOutcome::Deny);
        assert_eq!(
            decision.reason.as_deref(),
            Some(
                "this run does not allow writes — remembered from an earlier refusal, so asking again will not change it"
            )
        );
    }

    #[tokio::test]
    async fn a_refusal_remembered_without_a_reason_keeps_the_standing_wording() {
        let store = RuleStore::new();
        store.add_rule(shell_rule(false, None));

        let decision = answered_by_rule(store).await;

        assert_eq!(decision.outcome, ToolAuthorizationOutcome::Deny);
        assert_eq!(decision.reason.as_deref(), Some(BLOCKED_BY_REMEMBERED_RULE));
    }

    #[tokio::test]
    async fn a_remembered_allow_answers_without_words() {
        // Nothing writes a reason onto an allow, but the type permits one, and
        // an allowed call still explains itself by happening.
        let store = RuleStore::new();
        store.add_rule(shell_rule(true, Some("should never be read")));

        let decision = answered_by_rule(store).await;

        assert_eq!(decision.outcome, ToolAuthorizationOutcome::Allow);
        assert_eq!(decision.reason, None);
    }

    #[test]
    fn matching_rule_hands_back_the_reason_of_the_rule_that_won() {
        // Precedence decides which reason the model reads, so the pattern
        // rule's words must come back rather than the bare rule's.
        let store = RuleStore::new();
        store.add_rule(shell_rule(false, Some("shell is refused in this run")));
        store.add_rule(RememberedRule {
            key: RuleKey {
                tool_name: "shell".to_owned(),
                pattern: Some("**cargo test**".to_owned()),
            },
            allow: false,
            scope: PermissionRuleScope::Session,
            reason: Some("the test suite is not run from inside a run".to_owned()),
        });

        let matched = store
            .matching_rule("shell", Some(r#"{"command":"cargo test"}"#))
            .expect("a rule should match");

        assert_eq!(
            matched.reason.as_deref(),
            Some("the test suite is not run from inside a run")
        );
    }

    #[test]
    fn check_matches_tool_name_without_pattern() {
        let store = RuleStore::new();
        store.add_rule(RememberedRule {
            key: RuleKey {
                tool_name: "shell".to_owned(),
                pattern: None,
            },
            allow: true,
            scope: PermissionRuleScope::Session,
            reason: None,
        });
        // Bare rule (no pattern) matches regardless of input_json content.
        assert_eq!(
            store.check("shell", Some(r#"{"command":"ls"}"#)),
            Some(true)
        );
        assert_eq!(store.check("shell", None), Some(true));
    }

    #[test]
    fn check_matches_pattern_against_input_json() {
        let store = RuleStore::new();
        store.add_rule(RememberedRule {
            key: RuleKey {
                tool_name: "shell".to_owned(),
                pattern: Some("*cargo test*".to_owned()),
            },
            allow: true,
            scope: PermissionRuleScope::Session,
            reason: None,
        });
        assert_eq!(
            store.check("shell", Some(r#"{"command":"cargo test"}"#)),
            Some(true)
        );
    }

    #[test]
    fn check_pattern_rule_does_not_match_without_input() {
        let store = RuleStore::new();
        store.add_rule(RememberedRule {
            key: RuleKey {
                tool_name: "shell".to_owned(),
                pattern: Some("*cargo test*".to_owned()),
            },
            allow: true,
            scope: PermissionRuleScope::Session,
            reason: None,
        });
        // Pattern rule is ignored when input is None — no bare rule either,
        // so result must be None.
        assert_eq!(store.check("shell", None), None);
    }

    #[test]
    fn check_pattern_rule_takes_precedence_over_no_pattern() {
        let store = RuleStore::new();
        // Bare rule: allow.
        store.add_rule(RememberedRule {
            key: RuleKey {
                tool_name: "shell".to_owned(),
                pattern: None,
            },
            allow: true,
            scope: PermissionRuleScope::Session,
            reason: None,
        });
        // Pattern rule: deny when input matches.
        // Use ** so path separators inside the JSON string are matched.
        store.add_rule(RememberedRule {
            key: RuleKey {
                tool_name: "shell".to_owned(),
                pattern: Some("**rm -rf**".to_owned()),
            },
            allow: false,
            scope: PermissionRuleScope::Session,
            reason: None,
        });
        // Pattern match should win over the bare allow.
        assert_eq!(
            store.check("shell", Some(r#"{"command":"rm -rf /tmp"}"#)),
            Some(false)
        );
    }

    #[test]
    fn check_non_matching_pattern_falls_through() {
        let store = RuleStore::new();
        // Only a pattern rule is present; input does not match it.
        store.add_rule(RememberedRule {
            key: RuleKey {
                tool_name: "shell".to_owned(),
                pattern: Some("*cargo test*".to_owned()),
            },
            allow: true,
            scope: PermissionRuleScope::Session,
            reason: None,
        });
        // Non-matching input yields None (no bare fallback).
        assert_eq!(store.check("shell", Some(r#"{"command":"ls"}"#)), None);
    }
}
