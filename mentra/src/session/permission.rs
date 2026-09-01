mod pattern;

use std::cmp::Ordering as CmpOrdering;
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
    /// Wildcard pattern matched against the JSON encoding of the call's
    /// structured input, or `None` to answer every call to the tool.
    ///
    /// Matched as data rather than as a path: `*` matches any run of
    /// characters including `/`, `**` means the same as `*`, `?` matches one
    /// character, and every other character — JSON's braces, brackets and
    /// commas included — is literal. Matching is anchored, so a rule about a
    /// fragment is written `*fragment*`.
    ///
    /// Path-glob semantics were wrong here: `*` stopped at `/`, so any preview
    /// carrying an absolute path made every key serialized after it
    /// unmatchable, and a rule written against one silently answered nothing.
    pub pattern: Option<String>,
}

/// Exact in-memory identity of one remembered permission rule.
///
/// Scope is part of the address rather than metadata on the stored value, so
/// the same tool and pattern can carry independent session, project, and global
/// answers. Construct this from a listed [`RememberedRule`] with
/// `PermissionRuleAddress::from(&rule)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PermissionRuleAddress {
    pub scope: PermissionRuleScope,
    pub key: RuleKey,
}

/// A stored permission rule that was previously decided by the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RememberedRule {
    pub key: RuleKey,
    pub allow: bool,
    pub scope: PermissionRuleScope,
    /// Why the remembered refusal refused, in the words the model will read.
    ///
    /// A remembered rule answers a later `Prompt` without reaching the session
    /// approver again, so a rule that keeps the verdict and drops the reason
    /// lets the host explain itself exactly once: every repeat after that reads
    /// only that something was blocked. Written from
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

impl From<&RememberedRule> for PermissionRuleAddress {
    fn from(rule: &RememberedRule) -> Self {
        Self {
            scope: rule.scope,
            key: rule.key.clone(),
        }
    }
}

const SCOPE_PRECEDENCE: [PermissionRuleScope; 3] = [
    PermissionRuleScope::Session,
    PermissionRuleScope::Project,
    PermissionRuleScope::Global,
];

fn scope_rank(scope: PermissionRuleScope) -> u8 {
    match scope {
        PermissionRuleScope::Session => 0,
        PermissionRuleScope::Project => 1,
        PermissionRuleScope::Global => 2,
    }
}

fn compare_rule_keys(left: &RuleKey, right: &RuleKey) -> CmpOrdering {
    left.tool_name
        .cmp(&right.tool_name)
        .then_with(|| left.pattern.cmp(&right.pattern))
}

fn compare_rules_for_listing(left: &RememberedRule, right: &RememberedRule) -> CmpOrdering {
    scope_rank(left.scope)
        .cmp(&scope_rank(right.scope))
        .then_with(|| left.key.tool_name.cmp(&right.key.tool_name))
        .then_with(|| match (&left.key.pattern, &right.key.pattern) {
            // Patterned rules are considered before the bare fallback within
            // one scope and tool, matching lookup semantics.
            (Some(_), None) => CmpOrdering::Less,
            (None, Some(_)) => CmpOrdering::Greater,
            (left, right) => left.cmp(right),
        })
}

fn compare_pattern_candidates(
    left: (&PermissionRuleAddress, &RememberedRule),
    right: (&PermissionRuleAddress, &RememberedRule),
) -> CmpOrdering {
    // `false < true`, so a denial wins an overlapping-pattern tie. Exact
    // addresses are unique; stable RuleKey order breaks every remaining tie
    // without depending on HashMap iteration order.
    left.1
        .allow
        .cmp(&right.1.allow)
        .then_with(|| compare_rule_keys(&left.0.key, &right.0.key))
}

/// Thread-safe in-memory store for remembered permission rules.
///
/// Rules are addressed by [`PermissionRuleAddress`]. Lookup considers scopes
/// in session, project, then global order. Within one scope a matching pattern
/// precedes the bare rule; overlapping patterns prefer a denial and then stable
/// [`RuleKey`] order.
#[derive(Debug, Clone)]
pub struct RuleStore {
    inner: Arc<Mutex<HashMap<PermissionRuleAddress, RememberedRule>>>,
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

    /// Adds or overwrites the rule at its exact scope and key.
    pub fn add_rule(&self, rule: RememberedRule) {
        let mut rules = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        rules.insert(PermissionRuleAddress::from(&rule), rule);
    }

    /// Checks whether a tool is allowed by a remembered rule.
    ///
    /// Scopes are considered in session, project, then global order. Within one
    /// scope, pattern rules are matched against `input_json` with the wildcard
    /// syntax documented on [`RuleKey::pattern`] and take precedence over the
    /// bare (no-pattern) rule. Overlapping patterns prefer denial, then stable
    /// key order. Returns `Some(true)` if allowed, `Some(false)` if denied, or
    /// `None` if no matching rule exists.
    /// Use [`RuleStore::matching_rule`] when the rule's own reason matters.
    pub fn check(&self, tool_name: &str, input_json: Option<&str>) -> Option<bool> {
        self.matching_rule(tool_name, input_json)
            .map(|rule| rule.allow)
    }

    /// The remembered rule that answers a call, if one does.
    ///
    /// Matches exactly as [`RuleStore::check`] does and hands back the whole
    /// rule, so a refusal can restate the reason it was remembered with rather
    /// than only its verdict.
    pub fn matching_rule(
        &self,
        tool_name: &str,
        input_json: Option<&str>,
    ) -> Option<RememberedRule> {
        let rules = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        for scope in SCOPE_PRECEDENCE {
            if let Some((_, rule)) = rules
                .iter()
                .filter(|(address, _)| {
                    address.scope == scope
                        && address.key.tool_name == tool_name
                        && address.key.pattern.as_deref().is_some_and(|rule_pattern| {
                            input_json.is_some_and(|json| pattern::matches(rule_pattern, json))
                        })
                })
                .min_by(|left, right| compare_pattern_candidates(*left, *right))
            {
                return Some(rule.clone());
            }

            if let Some((_, rule)) = rules.iter().find(|(address, _)| {
                address.scope == scope
                    && address.key.tool_name == tool_name
                    && address.key.pattern.is_none()
            }) {
                return Some(rule.clone());
            }
        }

        None
    }

    /// Returns all remembered rules in deterministic lookup-oriented order.
    ///
    /// Session rules precede project and global rules. Within one scope, tools
    /// and patterns use stable lexical ordering, with patterned rules before a
    /// tool's bare fallback.
    pub fn rules(&self) -> Vec<RememberedRule> {
        let rules = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut listed: Vec<_> = rules.values().cloned().collect();
        listed.sort_by(compare_rules_for_listing);
        listed
    }

    /// Revokes the rule at `address`, returning whether one existed.
    pub fn revoke_rule(&self, address: &PermissionRuleAddress) -> bool {
        let mut rules = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        rules.remove(address).is_some()
    }

    /// Removes every rule at `scope`, returning how many were removed.
    pub fn clear_scope(&self, scope: PermissionRuleScope) -> usize {
        let mut rules = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let before = rules.len();
        rules.retain(|address, _| address.scope != scope);
        before - rules.len()
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
/// This is the bridge that first asks the current authorizer, then lets a
/// remembered rule answer only its `Prompt` outcome. An authoritative `Allow`
/// or `Deny` is returned unchanged. A prompt with no remembered answer becomes
/// a typed `SessionEvent::PermissionRequested` event and suspends execution
/// until a matching decision arrives.
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
        let Some(inner) = self.inner.as_ref().cloned() else {
            return Ok(ToolAuthorizationDecision::allow());
        };

        // Sample one authorizer for this call before awaiting it. A stateful
        // session policy may change while a permission dialog is open; that
        // change governs the next call, while this call keeps the policy that
        // decided it needed a prompt.
        let decision = inner.authorize(request).await?;
        if decision.outcome != ToolAuthorizationOutcome::Prompt {
            return Ok(decision);
        }

        let input_json = serde_json::to_string(&request.preview.structured_input).ok();
        if let Some(rule) = self
            .rule_store
            .matching_rule(&request.tool_name, input_json.as_deref())
        {
            return Ok(if rule.allow {
                ToolAuthorizationDecision::allow()
            } else {
                // The session approver is not consulted again, so the rule is
                // the only place the original reason can still come from.
                ToolAuthorizationDecision::deny(remembered_denial(rule.reason.as_deref()))
            });
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
            // Nothing downstream can work this out again: this is the last
            // layer holding the preview the authorizer was given.
            classification: Some(request.preview.classification()),
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
    use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

    use crate::tool::{
        ToolApprovalCategory, ToolAuthorizationPreview, ToolCapability, ToolClassification,
        ToolDurability, ToolExecutionCategory, ToolSideEffectLevel,
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

    #[derive(Clone)]
    struct CountingAuthorizer {
        outcome: ToolAuthorizationOutcome,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ToolAuthorizer for CountingAuthorizer {
        async fn authorize(
            &self,
            _request: &ToolAuthorizationRequest,
        ) -> Result<ToolAuthorizationDecision, RuntimeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(match self.outcome {
                ToolAuthorizationOutcome::Allow => ToolAuthorizationDecision::allow(),
                ToolAuthorizationOutcome::Prompt => {
                    ToolAuthorizationDecision::prompt("needs manual review")
                }
                ToolAuthorizationOutcome::Deny => {
                    ToolAuthorizationDecision::deny("the current policy refuses")
                }
            })
        }
    }

    #[derive(Clone)]
    struct SwitchingAuthorizer {
        outcome: Arc<AtomicU8>,
        calls: Arc<AtomicUsize>,
    }

    impl SwitchingAuthorizer {
        const PROMPT: u8 = 0;
        const DENY: u8 = 1;

        fn prompting() -> Self {
            Self {
                outcome: Arc::new(AtomicU8::new(Self::PROMPT)),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn deny(&self) {
            self.outcome.store(Self::DENY, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl ToolAuthorizer for SwitchingAuthorizer {
        async fn authorize(
            &self,
            _request: &ToolAuthorizationRequest,
        ) -> Result<ToolAuthorizationDecision, RuntimeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(if self.outcome.load(Ordering::SeqCst) == Self::PROMPT {
                ToolAuthorizationDecision::prompt("needs manual review")
            } else {
                ToolAuthorizationDecision::deny("the current policy refuses")
            })
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

    /// The classification is the one thing on this event nothing downstream
    /// can recompute: the session authorizer is the last layer holding the
    /// preview, and everything past it sees only the event.
    #[tokio::test]
    async fn the_emitted_request_carries_the_classification_the_authorizer_saw() {
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
        let SessionEvent::PermissionRequested {
            request_id,
            classification,
            ..
        } = event
        else {
            panic!("expected PermissionRequested, got {event:?}");
        };

        assert_eq!(
            classification.as_ref(),
            Some(&ToolClassification::from(&request.preview)),
            "every classification field the authorizer was given has to reach the event"
        );
        assert_eq!(
            classification.map(|classification| classification.side_effect_level),
            Some(ToolSideEffectLevel::Process),
            "a host reading only the event can tell a process launch from a local write"
        );

        pending
            .remove(&request_id)
            .expect("pending permission should be registered")
            .sender
            .send(PermissionDecision::allow())
            .expect("decision send should succeed");
        authorize_task.await.expect("task should succeed");
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

    /// Answers one prompted authorize call from `store`.
    async fn answered_by_rule(store: RuleStore) -> ToolAuthorizationDecision {
        let (event_tx, _rx) = broadcast::channel(8);
        let authorizer = SessionToolAuthorizer::new(
            Some(Arc::new(PromptAuthorizer)),
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
        rule_at(PermissionRuleScope::Session, "shell", None, allow, reason)
    }

    fn rule_at(
        scope: PermissionRuleScope,
        tool_name: &str,
        pattern: Option<&str>,
        allow: bool,
        reason: Option<&str>,
    ) -> RememberedRule {
        RememberedRule {
            key: RuleKey {
                tool_name: tool_name.to_owned(),
                pattern: pattern.map(str::to_owned),
            },
            allow,
            scope,
            reason: reason.map(str::to_owned),
        }
    }

    async fn current_policy_with_rule(
        outcome: ToolAuthorizationOutcome,
        rule: RememberedRule,
    ) -> (ToolAuthorizationDecision, usize, bool) {
        let calls = Arc::new(AtomicUsize::new(0));
        let store = RuleStore::new();
        store.add_rule(rule);
        let (event_tx, mut rx) = broadcast::channel(8);
        let authorizer = SessionToolAuthorizer::new(
            Some(Arc::new(CountingAuthorizer {
                outcome,
                calls: Arc::clone(&calls),
            })),
            event_tx,
            PendingPermissionStore::new(),
            store,
        );

        let decision = authorizer
            .authorize(&sample_request())
            .await
            .expect("authorization should resolve");
        let emitted = rx.try_recv().is_ok();
        (decision, calls.load(Ordering::SeqCst), emitted)
    }

    #[tokio::test]
    async fn a_current_denial_beats_a_remembered_allow() {
        let (decision, calls, emitted) =
            current_policy_with_rule(ToolAuthorizationOutcome::Deny, shell_rule(true, None)).await;

        assert_eq!(decision.outcome, ToolAuthorizationOutcome::Deny);
        assert_eq!(
            decision.reason.as_deref(),
            Some("the current policy refuses")
        );
        assert_eq!(calls, 1, "the current policy must be consulted first");
        assert!(!emitted, "a policy denial has nothing to ask about");
    }

    #[tokio::test]
    async fn a_current_allow_beats_a_remembered_denial() {
        let (decision, calls, emitted) = current_policy_with_rule(
            ToolAuthorizationOutcome::Allow,
            shell_rule(false, Some("an earlier policy refused")),
        )
        .await;

        assert_eq!(decision.outcome, ToolAuthorizationOutcome::Allow);
        assert_eq!(calls, 1, "the current policy must be consulted first");
        assert!(!emitted, "a policy allow has nothing to ask about");
    }

    #[tokio::test]
    async fn a_current_prompt_consults_a_matching_remembered_rule() {
        let (decision, calls, emitted) =
            current_policy_with_rule(ToolAuthorizationOutcome::Prompt, shell_rule(true, None))
                .await;

        assert_eq!(decision.outcome, ToolAuthorizationOutcome::Allow);
        assert_eq!(calls, 1, "the current policy must be consulted first");
        assert!(!emitted, "the remembered answer avoids a duplicate prompt");
    }

    #[tokio::test]
    async fn no_inner_authorizer_allows_even_with_a_remembered_denial() {
        let store = RuleStore::new();
        store.add_rule(shell_rule(false, Some("an earlier policy refused")));
        let (event_tx, mut rx) = broadcast::channel(8);
        let authorizer =
            SessionToolAuthorizer::new(None, event_tx, PendingPermissionStore::new(), store);

        let decision = authorizer
            .authorize(&sample_request())
            .await
            .expect("authorization should resolve");

        assert_eq!(decision.outcome, ToolAuthorizationOutcome::Allow);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_late_remembered_answer_applies_to_its_call_but_not_the_next_policy() {
        let inner = SwitchingAuthorizer::prompting();
        let store = RuleStore::new();
        let pending = PendingPermissionStore::new();
        let (event_tx, mut rx) = broadcast::channel(8);
        let authorizer = SessionToolAuthorizer::new(
            Some(Arc::new(inner.clone())),
            event_tx,
            pending.clone(),
            store.clone(),
        );

        let first = tokio::spawn({
            let authorizer = authorizer.clone();
            async move { authorizer.authorize(&sample_request()).await.unwrap() }
        });
        let event = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("permission request should arrive")
            .expect("event should be present");
        let SessionEvent::PermissionRequested { request_id, .. } = event else {
            panic!("expected PermissionRequested, got {event:?}");
        };

        inner.deny();
        store.add_rule(shell_rule(true, None));
        pending
            .remove(&request_id)
            .expect("pending permission should be registered")
            .sender
            .send(PermissionDecision::allow_and_remember(
                PermissionRuleScope::Session,
            ))
            .expect("decision send should succeed");

        let first = first
            .await
            .expect("first authorization task should succeed");
        assert_eq!(
            first.outcome,
            ToolAuthorizationOutcome::Allow,
            "the already-open request keeps the answer given to it"
        );

        let next = authorizer
            .authorize(&sample_request())
            .await
            .expect("next authorization should resolve");
        assert_eq!(next.outcome, ToolAuthorizationOutcome::Deny);
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
        assert!(
            rx.try_recv().is_err(),
            "the stricter next policy must not prompt or consult the stale allow"
        );
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
        // Pattern rule: deny when input matches. `**` reads the same as `*`
        // now that a pattern is matched as data, and is kept here because a
        // rule persisted with that spelling has to keep answering.
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

    /// The preview a host builds for a routed command, with its keys in the
    /// order `serde_json` writes them: an absolute `cwd` sits before `mode`
    /// and `target`.
    fn spawn_preview() -> &'static str {
        r#"{"body":"cargo test","cwd":"/Users/dev/basis","mode":"command","target":"mac"}"#
    }

    /// A pattern is matched against JSON, and JSON is not a path. Matched by a
    /// path globber, `*` stops dead at the `/` inside an absolute `cwd`, so
    /// every key serialized after `cwd` becomes unreachable — the rule saves,
    /// reports nothing, and silently answers no call it was written for.
    #[test]
    fn a_pattern_reaches_a_key_that_follows_an_absolute_path() {
        let store = RuleStore::new();
        store.add_rule(RememberedRule {
            key: RuleKey {
                tool_name: "spawn".to_owned(),
                pattern: Some(r#"**"mode":"command"**"#.to_owned()),
            },
            allow: true,
            scope: PermissionRuleScope::Session,
            reason: None,
        });

        assert_eq!(store.check("spawn", Some(spawn_preview())), Some(true));
    }

    #[test]
    fn a_pattern_reaches_the_last_key_of_a_preview() {
        let store = RuleStore::new();
        store.add_rule(RememberedRule {
            key: RuleKey {
                tool_name: "spawn".to_owned(),
                pattern: Some(r#"**"target":"mac"**"#.to_owned()),
            },
            allow: true,
            scope: PermissionRuleScope::Session,
            reason: None,
        });

        assert_eq!(store.check("spawn", Some(spawn_preview())), Some(true));
    }

    /// `**` was only ever needed because `*` could not cross a separator.
    /// Both now mean the same thing, so a rule written either way answers.
    #[test]
    fn one_star_and_two_stars_both_cross_a_path_separator() {
        let store = RuleStore::new();
        store.add_rule(RememberedRule {
            key: RuleKey {
                tool_name: "spawn".to_owned(),
                pattern: Some(r#"*"target":"mac"*"#.to_owned()),
            },
            allow: true,
            scope: PermissionRuleScope::Session,
            reason: None,
        });

        assert_eq!(store.check("spawn", Some(spawn_preview())), Some(true));
    }

    /// JSON is punctuation-dense, and a path globber reads some of that
    /// punctuation as syntax: `{`…`}` is brace alternation and `[`…`]` a
    /// character class. A pattern that quotes the front of an object must
    /// match the object it quotes.
    #[test]
    fn json_punctuation_in_a_pattern_is_matched_literally() {
        let store = RuleStore::new();
        store.add_rule(RememberedRule {
            key: RuleKey {
                tool_name: "spawn".to_owned(),
                pattern: Some(r#"{"body":"cargo test"*"#.to_owned()),
            },
            allow: true,
            scope: PermissionRuleScope::Session,
            reason: None,
        });

        assert_eq!(store.check("spawn", Some(spawn_preview())), Some(true));
    }

    #[test]
    fn a_pattern_that_names_another_target_does_not_match() {
        let store = RuleStore::new();
        store.add_rule(RememberedRule {
            key: RuleKey {
                tool_name: "spawn".to_owned(),
                pattern: Some(r#"**"target":"linux"**"#.to_owned()),
            },
            allow: true,
            scope: PermissionRuleScope::Session,
            reason: None,
        });

        assert_eq!(store.check("spawn", Some(spawn_preview())), None);
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

    #[test]
    fn the_same_key_coexists_at_every_scope_and_the_narrowest_scope_wins() {
        let store = RuleStore::new();
        store.add_rule(rule_at(
            PermissionRuleScope::Global,
            "shell",
            None,
            false,
            Some("global"),
        ));
        store.add_rule(rule_at(
            PermissionRuleScope::Project,
            "shell",
            None,
            false,
            Some("project"),
        ));
        store.add_rule(rule_at(
            PermissionRuleScope::Session,
            "shell",
            None,
            true,
            Some("session"),
        ));

        assert_eq!(store.rules().len(), 3);
        let matched = store
            .matching_rule("shell", None)
            .expect("one scoped rule should match");
        assert_eq!(matched.scope, PermissionRuleScope::Session);
        assert_eq!(matched.reason.as_deref(), Some("session"));
    }

    #[test]
    fn scope_precedence_is_applied_before_pattern_precedence() {
        let store = RuleStore::new();
        store.add_rule(rule_at(
            PermissionRuleScope::Global,
            "shell",
            Some("*cargo test*"),
            false,
            Some("global pattern"),
        ));
        store.add_rule(rule_at(
            PermissionRuleScope::Session,
            "shell",
            None,
            true,
            Some("session bare"),
        ));

        let matched = store
            .matching_rule("shell", Some(r#"{"command":"cargo test"}"#))
            .expect("one scoped rule should match");
        assert_eq!(matched.scope, PermissionRuleScope::Session);
        assert_eq!(matched.reason.as_deref(), Some("session bare"));
    }

    #[test]
    fn overlapping_patterns_prefer_denial_then_stable_key_order() {
        fn populated(
            patterns: impl IntoIterator<Item = (&'static str, bool, &'static str)>,
        ) -> RuleStore {
            let store = RuleStore::new();
            for (pattern, allow, reason) in patterns {
                store.add_rule(rule_at(
                    PermissionRuleScope::Project,
                    "shell",
                    Some(pattern),
                    allow,
                    Some(reason),
                ));
            }
            store
        }

        let rules = [
            ("*test*", false, "deny test"),
            ("*cargo*", false, "deny cargo"),
            ("*cargo test*", true, "allow exact phrase"),
        ];
        let forward = populated(rules);
        let reverse = populated(rules.into_iter().rev());

        for store in [forward, reverse] {
            let matched = store
                .matching_rule("shell", Some(r#"{"command":"cargo test"}"#))
                .expect("one pattern should win");
            assert!(!matched.allow, "a denial wins an overlapping tie");
            assert_eq!(
                matched.key.pattern.as_deref(),
                Some("*cargo*"),
                "equally denying matches use stable RuleKey order"
            );
            assert_eq!(matched.reason.as_deref(), Some("deny cargo"));
        }
    }

    #[test]
    fn exact_revoke_is_idempotent_and_leaves_other_addresses() {
        let store = RuleStore::new();
        for scope in [
            PermissionRuleScope::Global,
            PermissionRuleScope::Project,
            PermissionRuleScope::Session,
        ] {
            store.add_rule(rule_at(scope, "shell", None, true, None));
        }
        store.add_rule(rule_at(
            PermissionRuleScope::Session,
            "files",
            None,
            false,
            None,
        ));
        let project_shell = PermissionRuleAddress {
            scope: PermissionRuleScope::Project,
            key: RuleKey {
                tool_name: "shell".to_owned(),
                pattern: None,
            },
        };

        assert!(store.revoke_rule(&project_shell));
        assert!(!store.revoke_rule(&project_shell));
        let rules = store.rules();
        assert_eq!(rules.len(), 3);
        assert!(rules.iter().any(|rule| {
            rule.scope == PermissionRuleScope::Global && rule.key.tool_name == "shell"
        }));
        assert!(rules.iter().any(|rule| {
            rule.scope == PermissionRuleScope::Session && rule.key.tool_name == "shell"
        }));
        assert!(rules.iter().any(|rule| rule.key.tool_name == "files"));
    }

    #[test]
    fn clear_scope_returns_the_number_removed() {
        let store = RuleStore::new();
        store.add_rule(rule_at(
            PermissionRuleScope::Session,
            "shell",
            None,
            true,
            None,
        ));
        store.add_rule(rule_at(
            PermissionRuleScope::Session,
            "files",
            None,
            false,
            None,
        ));
        store.add_rule(rule_at(
            PermissionRuleScope::Project,
            "shell",
            None,
            false,
            None,
        ));

        assert_eq!(store.clear_scope(PermissionRuleScope::Session), 2);
        assert_eq!(store.clear_scope(PermissionRuleScope::Session), 0);
        assert_eq!(store.rules().len(), 1);
        assert_eq!(store.rules()[0].scope, PermissionRuleScope::Project);
    }

    #[test]
    fn rules_are_listed_in_semantic_then_stable_key_order() {
        let store = RuleStore::new();
        store.add_rule(rule_at(
            PermissionRuleScope::Global,
            "shell",
            None,
            true,
            None,
        ));
        store.add_rule(rule_at(
            PermissionRuleScope::Session,
            "shell",
            None,
            true,
            None,
        ));
        store.add_rule(rule_at(
            PermissionRuleScope::Session,
            "files",
            Some("*read*"),
            true,
            None,
        ));
        store.add_rule(rule_at(
            PermissionRuleScope::Session,
            "files",
            None,
            false,
            None,
        ));
        store.add_rule(rule_at(
            PermissionRuleScope::Project,
            "shell",
            None,
            false,
            None,
        ));

        let listed: Vec<_> = store
            .rules()
            .into_iter()
            .map(|rule| (rule.scope, rule.key.tool_name, rule.key.pattern))
            .collect();
        assert_eq!(
            listed,
            vec![
                (
                    PermissionRuleScope::Session,
                    "files".to_owned(),
                    Some("*read*".to_owned()),
                ),
                (PermissionRuleScope::Session, "files".to_owned(), None,),
                (PermissionRuleScope::Session, "shell".to_owned(), None),
                (PermissionRuleScope::Project, "shell".to_owned(), None),
                (PermissionRuleScope::Global, "shell".to_owned(), None),
            ]
        );
    }
}
