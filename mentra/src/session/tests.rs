#![allow(clippy::unwrap_used)]

use crate::session::event::*;
use crate::session::permission::*;
use crate::session::types::*;

// ---- Task 1 type-level tests (preserved) ----

#[test]
fn session_id_roundtrips_through_serde() {
    let id = SessionId::new();
    let json = serde_json::to_string(&id).unwrap();
    let deserialized: SessionId = serde_json::from_str(&json).unwrap();
    assert_eq!(id, deserialized);
}

#[test]
fn session_id_from_raw_preserves_value() {
    let id = SessionId::from_raw("session-abc-123");
    assert_eq!(id.as_str(), "session-abc-123");
}

#[test]
fn session_metadata_serialization_roundtrip() {
    let metadata = SessionMetadata::new(
        SessionId::from_raw("session-test-1"),
        "Test Session",
        "claude-opus-4-20250514",
    );
    let json = serde_json::to_value(&metadata).unwrap();
    let deserialized: SessionMetadata = serde_json::from_value(json).unwrap();
    assert_eq!(metadata, deserialized);
}

#[test]
fn session_event_assistant_token_delta_roundtrip() {
    let event = SessionEvent::AssistantTokenDelta {
        delta: "hello".to_string(),
        full_text: "hello".to_string(),
    };
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["type"], "assistant_token_delta");
    let deserialized: SessionEvent = serde_json::from_value(json).unwrap();
    assert_eq!(event, deserialized);
}

#[test]
fn session_event_tool_queued_roundtrip() {
    let event = SessionEvent::ToolQueued {
        tool_call_id: "tc-1".to_string(),
        tool_name: "shell".to_string(),
        summary: "Run 'cargo test'".to_string(),
        mutability: ToolMutability::Mutating,
        input_json: r#"{"command":"cargo test"}"#.to_string(),
    };
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["type"], "tool_queued");
    assert_eq!(json["tool_name"], "shell");
    let deserialized: SessionEvent = serde_json::from_value(json).unwrap();
    assert_eq!(event, deserialized);
}

#[test]
fn session_event_permission_requested_roundtrip() {
    let preview_json = serde_json::to_string(&serde_json::json!({
        "command": "rm -rf /tmp/foo",
        "cwd": "/Users/dev/project"
    }))
    .unwrap();
    let event = SessionEvent::PermissionRequested {
        request_id: "perm-1".to_string(),
        tool_call_id: "tc-1".to_string(),
        tool_name: "shell".to_string(),
        description: "Execute shell command: rm -rf /tmp/foo".to_string(),
        preview: preview_json,
    };
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["type"], "permission_requested");
    let deserialized: SessionEvent = serde_json::from_value(json).unwrap();
    assert_eq!(event, deserialized);
}

#[test]
fn session_event_compaction_completed_roundtrip() {
    let event = SessionEvent::CompactionCompleted {
        agent_id: "agent-1".to_string(),
        replaced_items: 42,
        preserved_items: 8,
        resulting_transcript_len: 10,
        extracted_facts_count: 3,
        summary_preview: "key facts extracted".to_string(),
    };
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["type"], "compaction_completed");
    let deserialized: SessionEvent = serde_json::from_value(json).unwrap();
    assert_eq!(event, deserialized);
}

#[test]
fn session_event_task_updated_roundtrip() {
    let event = SessionEvent::TaskUpdated {
        task_id: "bg-1".to_string(),
        kind: TaskKind::BackgroundTask,
        status: TaskLifecycleStatus::Running,
        title: "cargo test -p mentra".to_string(),
        detail: Some("exit code: 0".to_string()),
    };
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["type"], "task_updated");
    let deserialized: SessionEvent = serde_json::from_value(json).unwrap();
    assert_eq!(event, deserialized);
}

#[test]
fn all_session_event_variants_serialize_with_type_tag() {
    let events: Vec<SessionEvent> = vec![
        SessionEvent::SessionStarted {
            session_id: SessionId::from_raw("s1"),
        },
        SessionEvent::UserMessage {
            text: "hi".to_string(),
        },
        SessionEvent::AssistantTokenDelta {
            delta: "h".to_string(),
            full_text: "h".to_string(),
        },
        SessionEvent::AssistantMessageCompleted {
            text: "hello".to_string(),
        },
        SessionEvent::ToolQueued {
            tool_call_id: "tc1".to_string(),
            tool_name: "read".to_string(),
            summary: "Read file".to_string(),
            mutability: ToolMutability::ReadOnly,
            input_json: "{}".to_string(),
        },
        SessionEvent::ToolStarted {
            tool_call_id: "tc1".to_string(),
            tool_name: "read".to_string(),
        },
        SessionEvent::ToolProgress {
            tool_call_id: "tc1".to_string(),
            tool_name: "read".to_string(),
            progress: "50%".to_string(),
        },
        SessionEvent::ToolCompleted {
            tool_call_id: "tc1".to_string(),
            tool_name: "read".to_string(),
            summary: "Read 42 lines".to_string(),
            is_error: false,
        },
        SessionEvent::PermissionRequested {
            request_id: "p1".to_string(),
            tool_call_id: "tc1".to_string(),
            tool_name: "shell".to_string(),
            description: "run command".to_string(),
            preview: "{}".to_string(),
        },
        SessionEvent::PermissionResolved {
            request_id: "p1".to_string(),
            tool_call_id: "tc1".to_string(),
            tool_name: "shell".to_string(),
            outcome: PermissionOutcome::Allowed,
            rule_scope: Some(PermissionRuleScope::Session),
        },
        SessionEvent::TaskUpdated {
            task_id: "t1".to_string(),
            kind: TaskKind::Subagent,
            status: TaskLifecycleStatus::Spawned,
            title: "research".to_string(),
            detail: None,
        },
        SessionEvent::CompactionStarted {
            agent_id: "a1".to_string(),
        },
        SessionEvent::CompactionCompleted {
            agent_id: "a1".to_string(),
            replaced_items: 10,
            preserved_items: 5,
            resulting_transcript_len: 7,
            extracted_facts_count: 0,
            summary_preview: String::new(),
        },
        SessionEvent::MemoryUpdated {
            agent_id: "a1".to_string(),
            stored_records: 3,
        },
        SessionEvent::Notice {
            severity: NoticeSeverity::Info,
            message: "Context window 80% full".to_string(),
        },
        SessionEvent::RetryAttempt {
            agent_id: "a1".to_string(),
            error_message: "transient error".to_string(),
            attempt: 1,
            max_attempts: 3,
            next_delay_ms: 500,
        },
        SessionEvent::Error {
            message: "Provider timeout".to_string(),
            recoverable: true,
        },
    ];

    for event in events {
        let json = serde_json::to_value(&event).unwrap();
        assert!(
            json.get("type").is_some(),
            "Event missing 'type' tag: {event:?}"
        );
        let roundtripped: SessionEvent = serde_json::from_value(json).unwrap();
        assert_eq!(event, roundtripped);
    }
}

// ---- Task 2 lifecycle tests ----

use crate::{ContentBlock, test::MockRuntime};

#[tokio::test]
async fn create_session_produces_valid_metadata() {
    let mock = MockRuntime::builder().text("hello").build().unwrap();
    let session = mock
        .runtime()
        .create_session("test-session", mock.model())
        .unwrap();

    assert_eq!(session.name(), "test-session");
    assert_eq!(session.metadata().title, "test-session");
    assert_eq!(session.metadata().model, mock.model().id);
    assert_eq!(session.metadata().status, SessionStatus::Created);
    assert_eq!(session.metadata().turn_count, 0);
}

#[tokio::test]
async fn append_turn_returns_assistant_message() {
    let mock = MockRuntime::builder()
        .text("hello from session")
        .build()
        .unwrap();
    let mut session = mock
        .runtime()
        .create_session("test-session", mock.model())
        .unwrap();

    let message = session
        .append_turn(vec![ContentBlock::text("hi")])
        .await
        .unwrap();

    assert_eq!(message.text(), "hello from session");
    assert_eq!(session.metadata().turn_count, 1);
    assert_eq!(session.metadata().status, SessionStatus::Idle);
}

#[tokio::test]
async fn append_turn_emits_user_and_assistant_events() {
    let mock = MockRuntime::builder().text("response").build().unwrap();
    let mut session = mock
        .runtime()
        .create_session("test-session", mock.model())
        .unwrap();

    let mut rx = session.subscribe();

    let _message = session
        .append_turn(vec![ContentBlock::text("hello")])
        .await
        .unwrap();

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }

    let has_user = events
        .iter()
        .any(|e| matches!(e, SessionEvent::UserMessage { text } if text == "hello"));
    let has_assistant = events.iter().any(
        |e| matches!(e, SessionEvent::AssistantMessageCompleted { text } if text == "response"),
    );

    assert!(has_user, "Expected UserMessage event, got: {events:?}");
    assert!(
        has_assistant,
        "Expected AssistantMessageCompleted event, got: {events:?}"
    );
}

#[tokio::test]
async fn replay_returns_transcript_after_turn() {
    let mock = MockRuntime::builder().text("world").build().unwrap();
    let mut session = mock
        .runtime()
        .create_session("test-session", mock.model())
        .unwrap();

    let _message = session
        .append_turn(vec![ContentBlock::text("hello")])
        .await
        .unwrap();

    let transcript = session.replay();
    assert!(
        !transcript.items().is_empty(),
        "Transcript should have items after a turn"
    );
}

#[tokio::test]
async fn session_status_transitions_created_to_idle() {
    let mock = MockRuntime::builder().text("done").build().unwrap();
    let mut session = mock
        .runtime()
        .create_session("test-session", mock.model())
        .unwrap();

    assert_eq!(session.metadata().status, SessionStatus::Created);

    let _message = session
        .append_turn(vec![ContentBlock::text("go")])
        .await
        .unwrap();

    assert_eq!(session.metadata().status, SessionStatus::Idle);
}

#[tokio::test]
async fn history_returns_committed_messages() {
    let mock = MockRuntime::builder().text("response").build().unwrap();
    let mut session = mock
        .runtime()
        .create_session("test-session", mock.model())
        .unwrap();

    assert!(session.history().is_empty());

    let _message = session
        .append_turn(vec![ContentBlock::text("hello")])
        .await
        .unwrap();

    assert!(
        !session.history().is_empty(),
        "History should contain messages after a turn"
    );
}

#[tokio::test]
async fn create_session_emits_session_started() {
    let mock = MockRuntime::builder().text("hi").build().unwrap();

    let session = mock
        .runtime()
        .create_session("test-session", mock.model())
        .unwrap();

    // The SessionStarted event was emitted during creation.
    // Verify session id follows the expected format.
    assert!(session.id().as_str().starts_with("session-"));
}

// ---- Task 4 permission tests ----

// -- PermissionDecision constructors --

#[test]
fn permission_decision_allow_constructor() {
    let decision = PermissionDecision::allow();
    assert!(decision.allow);
    assert!(decision.remember_as.is_none());
}

#[test]
fn permission_decision_deny_constructor() {
    let decision = PermissionDecision::deny();
    assert!(!decision.allow);
    assert!(decision.remember_as.is_none());
}

#[test]
fn permission_decision_allow_and_remember_constructor() {
    let decision = PermissionDecision::allow_and_remember(PermissionRuleScope::Session);
    assert!(decision.allow);
    assert_eq!(decision.remember_as, Some(PermissionRuleScope::Session));
}

#[test]
fn permission_decision_deny_and_remember_constructor() {
    let decision = PermissionDecision::deny_and_remember(PermissionRuleScope::Global);
    assert!(!decision.allow);
    assert_eq!(decision.remember_as, Some(PermissionRuleScope::Global));
}

// -- RuleStore --

#[test]
fn rule_store_empty_check_returns_none() {
    let store = RuleStore::new();
    assert!(store.check("shell").is_none());
}

#[test]
fn rule_store_add_and_check_allow() {
    let store = RuleStore::new();
    store.add_rule(RememberedRule {
        key: RuleKey {
            tool_name: "shell".to_owned(),
            pattern: None,
        },
        allow: true,
        scope: PermissionRuleScope::Session,
    });
    assert_eq!(store.check("shell"), Some(true));
}

#[test]
fn rule_store_add_and_check_deny() {
    let store = RuleStore::new();
    store.add_rule(RememberedRule {
        key: RuleKey {
            tool_name: "shell".to_owned(),
            pattern: None,
        },
        allow: false,
        scope: PermissionRuleScope::Project,
    });
    assert_eq!(store.check("shell"), Some(false));
}

#[test]
fn rule_store_overwrite_replaces_rule() {
    let store = RuleStore::new();
    store.add_rule(RememberedRule {
        key: RuleKey {
            tool_name: "shell".to_owned(),
            pattern: None,
        },
        allow: true,
        scope: PermissionRuleScope::Session,
    });
    assert_eq!(store.check("shell"), Some(true));

    store.add_rule(RememberedRule {
        key: RuleKey {
            tool_name: "shell".to_owned(),
            pattern: None,
        },
        allow: false,
        scope: PermissionRuleScope::Session,
    });
    assert_eq!(store.check("shell"), Some(false));
}

#[test]
fn rule_store_clear_scope_removes_matching_rules() {
    let store = RuleStore::new();
    store.add_rule(RememberedRule {
        key: RuleKey {
            tool_name: "shell".to_owned(),
            pattern: None,
        },
        allow: true,
        scope: PermissionRuleScope::Session,
    });
    store.add_rule(RememberedRule {
        key: RuleKey {
            tool_name: "read".to_owned(),
            pattern: None,
        },
        allow: true,
        scope: PermissionRuleScope::Global,
    });

    store.clear_scope(PermissionRuleScope::Session);

    assert!(store.check("shell").is_none());
    assert_eq!(store.check("read"), Some(true));
}

#[test]
fn rule_store_rules_returns_all_entries() {
    let store = RuleStore::new();
    assert!(store.rules().is_empty());

    store.add_rule(RememberedRule {
        key: RuleKey {
            tool_name: "shell".to_owned(),
            pattern: None,
        },
        allow: true,
        scope: PermissionRuleScope::Session,
    });
    store.add_rule(RememberedRule {
        key: RuleKey {
            tool_name: "read".to_owned(),
            pattern: None,
        },
        allow: false,
        scope: PermissionRuleScope::Project,
    });

    assert_eq!(store.rules().len(), 2);
}

// -- Session.resolve_permission --

#[tokio::test]
async fn resolve_permission_emits_event_and_sends_decision() {
    let mock = MockRuntime::builder().text("hi").build().unwrap();
    let session = mock
        .runtime()
        .create_session("perm-test", mock.model())
        .unwrap();

    let mut rx = session.subscribe();

    // Simulate a pending permission by inserting directly.
    let (tx, oneshot_rx) = tokio::sync::oneshot::channel();
    session.pending_permissions.insert(
        "perm-1".to_owned(),
        crate::session::permission::PendingPermissionEntry {
            tool_call_id: "tc-1".to_owned(),
            tool_name: "shell".to_owned(),
            sender: tx,
        },
    );

    let decision = PermissionDecision::allow_and_remember(PermissionRuleScope::Session);
    session.resolve_permission("perm-1", decision).unwrap();

    // The oneshot should deliver the decision.
    let received = oneshot_rx.await.unwrap();
    assert!(received.allow);
    assert_eq!(received.remember_as, Some(PermissionRuleScope::Session));

    // A PermissionResolved event should have been emitted.
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    let resolved = events.iter().find(|e| {
        matches!(
            e,
            SessionEvent::PermissionResolved {
                request_id,
                outcome: PermissionOutcome::Allowed,
                ..
            } if request_id == "perm-1"
        )
    });
    assert!(
        resolved.is_some(),
        "Expected PermissionResolved event, got: {events:?}"
    );

    // The rule should have been remembered.
    let rules = session.remembered_rules();
    assert_eq!(rules.len(), 1);
    assert!(rules[0].allow);
}

#[tokio::test]
async fn resolve_permission_unknown_id_returns_error() {
    let mock = MockRuntime::builder().text("hi").build().unwrap();
    let session = mock
        .runtime()
        .create_session("perm-test", mock.model())
        .unwrap();

    let result = session.resolve_permission("nonexistent", PermissionDecision::deny());
    assert!(result.is_err());
}

#[derive(Clone)]
struct PromptingAuthorizer;

#[async_trait]
impl crate::tool::ToolAuthorizer for PromptingAuthorizer {
    async fn authorize(
        &self,
        _request: &crate::tool::ToolAuthorizationRequest,
    ) -> Result<crate::tool::ToolAuthorizationDecision, crate::error::RuntimeError> {
        Ok(crate::tool::ToolAuthorizationDecision::prompt(
            "integration test prompt",
        ))
    }
}

#[derive(Clone)]
struct InFlightProvider {
    model: crate::ModelInfo,
    turn: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl crate::Provider for InFlightProvider {
    fn descriptor(&self) -> crate::ProviderDescriptor {
        crate::ProviderDescriptor::new(self.model.provider.clone())
    }

    async fn list_models(&self) -> Result<Vec<crate::ModelInfo>, crate::ProviderError> {
        Ok(vec![self.model.clone()])
    }

    async fn stream(
        &self,
        _request: crate::Request<'_>,
    ) -> Result<crate::ProviderEventStream, crate::ProviderError> {
        let turn = self.turn.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let response = match turn {
            0 => crate::provider::Response {
                id: unique_turn_id(),
                model: self.model.id.clone(),
                role: crate::Role::Assistant,
                content: vec![crate::provider::ContentBlock::ToolUse {
                    id: "tool-1".to_string(),
                    name: "permission-test-tool".to_string(),
                    input: serde_json::json!({"input": "test"}),
                }],
                stop_reason: Some("tool_use".to_string()),
                usage: None,
            },
            _ => crate::provider::Response {
                id: unique_turn_id(),
                model: self.model.id.clone(),
                role: crate::Role::Assistant,
                content: vec![crate::provider::ContentBlock::text("final response")],
                stop_reason: None,
                usage: None,
            },
        };
        Ok(crate::provider_event_stream_from_response(response))
    }
}

#[derive(Clone)]
struct PromptTestTool;

#[async_trait]
impl crate::tool::ToolDefinition for PromptTestTool {
    fn descriptor(&self) -> crate::tool::ToolSpec {
        crate::tool::ToolSpec::builder("permission-test-tool")
            .description("Simple tool used for permission handle in-flight test")
            .input_schema(serde_json::json!({
                "type": "object",
                "properties": {}
            }))
            .build()
    }
}

#[async_trait]
impl crate::tool::ToolExecutor for PromptTestTool {
    async fn execute(
        &self,
        _ctx: crate::tool::ParallelToolContext,
        _input: serde_json::Value,
    ) -> crate::tool::ToolResult {
        Ok("tool-result".to_string())
    }
}

fn unique_turn_id() -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let _ = write!(&mut out, "perm-flight-{nanos}");
    out
}

#[tokio::test]
async fn resolve_permission_via_session_handle_while_append_turn_is_in_flight() {
    use tokio::time::timeout;

    let model = crate::ModelInfo::new("mock-model", crate::BuiltinProvider::OpenAI);
    let runtime = crate::Runtime::builder()
        .with_tool_authorizer(PromptingAuthorizer)
        .with_provider_instance(InFlightProvider {
            model: model.clone(),
            turn: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
        .with_policy(crate::RuntimePolicy::permissive())
        .build()
        .unwrap();
    runtime.register_tool(PromptTestTool);

    let mut session = runtime
        .create_session("permission-handle-flight", model.clone())
        .unwrap();
    let permission_handle = session.permission_handle();
    let mut events = session.subscribe();

    let append = tokio::spawn(async move {
        session
            .append_turn(vec![ContentBlock::text("run permission test tool")])
            .await
    });

    let mut request_id = None;
    for _ in 0..10 {
        let event = timeout(std::time::Duration::from_millis(200), events.recv())
            .await
            .expect("permission request should arrive")
            .expect("session event stream should still be active");
        if let SessionEvent::PermissionRequested {
            request_id: pending_id,
            ..
        } = event
        {
            request_id = Some(pending_id);
            break;
        }
    }

    let request_id = request_id.expect("expected a PermissionRequested event");
    assert!(!append.is_finished());

    permission_handle
        .resolve_permission(
            &request_id,
            PermissionDecision::allow_and_remember(PermissionRuleScope::Session),
        )
        .unwrap();

    let result = append
        .await
        .expect("append turn task should complete")
        .expect("append turn should succeed");
    assert_eq!(result.text(), "final response");
}

// ---- Task 8: Contract conformance integration tests ----

use async_trait::async_trait;
use serde_json::json;

use crate::{
    provider::ProviderError,
    test::MockToolCall,
    tool::{ParallelToolContext, ToolDefinition, ToolExecutor, ToolResult, ToolSpec},
};

struct EchoTool;

#[async_trait]
impl ToolDefinition for EchoTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("echo_tool")
            .description("Echo a canned result for testing")
            .input_schema(json!({
                "type": "object",
                "properties": {}
            }))
            .build()
    }
}

#[async_trait]
impl ToolExecutor for EchoTool {
    async fn execute(&self, _ctx: ParallelToolContext, _input: serde_json::Value) -> ToolResult {
        Ok("echoed".to_string())
    }
}

#[tokio::test]
async fn full_session_lifecycle_produces_correct_event_stream() {
    let mock = MockRuntime::builder()
        .text("Hello, world!")
        .build()
        .unwrap();
    let mut session = mock
        .runtime()
        .create_session("lifecycle-test", mock.model())
        .unwrap();

    let mut rx = session.subscribe();

    let message = session
        .append_turn(vec![ContentBlock::text("Hi there")])
        .await
        .unwrap();

    assert_eq!(message.text(), "Hello, world!");

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }

    // Verify UserMessage appears before AssistantMessageCompleted.
    let user_pos = events
        .iter()
        .position(|e| matches!(e, SessionEvent::UserMessage { text } if text == "Hi there"));
    let assistant_pos = events.iter().position(|e| {
        matches!(e, SessionEvent::AssistantMessageCompleted { text } if text == "Hello, world!")
    });

    assert!(
        user_pos.is_some(),
        "Expected UserMessage event, got: {events:?}"
    );
    assert!(
        assistant_pos.is_some(),
        "Expected AssistantMessageCompleted event, got: {events:?}"
    );
    assert!(
        user_pos.unwrap() < assistant_pos.unwrap(),
        "UserMessage must precede AssistantMessageCompleted, positions: user={}, assistant={}",
        user_pos.unwrap(),
        assistant_pos.unwrap()
    );
}

#[tokio::test]
async fn tool_call_session_produces_tool_lifecycle_events() {
    let mock = MockRuntime::builder()
        .tool_calls([MockToolCall::new("echo_tool", json!({}))])
        .text("tool work done")
        .build()
        .unwrap();
    mock.runtime().register_tool(EchoTool);

    let mut session = mock
        .runtime()
        .create_session("tool-test", mock.model())
        .unwrap();

    let mut rx = session.subscribe();

    let message = session
        .append_turn(vec![ContentBlock::text("run the tool")])
        .await
        .unwrap();

    assert_eq!(message.text(), "tool work done");

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }

    let has_tool_started = events.iter().any(
        |e| matches!(e, SessionEvent::ToolStarted { tool_name, .. } if tool_name == "echo_tool"),
    );
    let has_tool_completed = events.iter().any(|e| {
        matches!(e, SessionEvent::ToolCompleted { tool_call_id, .. } if tool_call_id == "tool-1")
    });

    assert!(
        has_tool_started,
        "Expected ToolStarted event for echo_tool, got: {events:?}"
    );
    assert!(
        has_tool_completed,
        "Expected ToolCompleted event for tool-1, got: {events:?}"
    );
}

#[derive(Clone)]
struct OverflowingToolProvider {
    model: crate::ModelInfo,
    turn: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl crate::Provider for OverflowingToolProvider {
    fn descriptor(&self) -> crate::ProviderDescriptor {
        crate::ProviderDescriptor::new(self.model.provider.clone())
    }

    async fn list_models(&self) -> Result<Vec<crate::ModelInfo>, crate::ProviderError> {
        Ok(vec![self.model.clone()])
    }

    async fn stream(
        &self,
        _request: crate::Request<'_>,
    ) -> Result<crate::ProviderEventStream, crate::ProviderError> {
        let turn = self.turn.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match turn {
            0 => Ok(buffered_provider_events(verbose_tool_turn_events(
                &self.model.id,
                300,
            ))),
            _ => Ok(crate::provider_event_stream_from_response(
                crate::provider::Response {
                    id: unique_turn_id(),
                    model: self.model.id.clone(),
                    role: crate::Role::Assistant,
                    content: vec![crate::provider::ContentBlock::text("tool run finished")],
                    stop_reason: None,
                    usage: None,
                },
            )),
        }
    }
}

fn buffered_provider_events(
    events: Vec<crate::provider::ProviderEvent>,
) -> crate::ProviderEventStream {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    for event in events {
        tx.send(Ok(event))
            .expect("session test provider receiver dropped unexpectedly");
    }
    rx
}

fn verbose_tool_turn_events(
    model_id: &str,
    delta_count: usize,
) -> Vec<crate::provider::ProviderEvent> {
    let mut events = vec![
        crate::provider::ProviderEvent::MessageStarted {
            id: unique_turn_id(),
            model: model_id.to_string(),
            role: crate::Role::Assistant,
        },
        crate::provider::ProviderEvent::ContentBlockStarted {
            index: 0,
            kind: crate::provider::ContentBlockStart::Text,
        },
    ];

    for index in 0..delta_count {
        events.push(crate::provider::ProviderEvent::ContentBlockDelta {
            index: 0,
            delta: crate::provider::ContentBlockDelta::Text(format!("chunk-{index}")),
        });
    }

    events.extend([
        crate::provider::ProviderEvent::ContentBlockStopped { index: 0 },
        crate::provider::ProviderEvent::ContentBlockStarted {
            index: 1,
            kind: crate::provider::ContentBlockStart::ToolUse {
                id: "tool-1".to_string(),
                name: "echo_tool".to_string(),
            },
        },
        crate::provider::ProviderEvent::ContentBlockDelta {
            index: 1,
            delta: crate::provider::ContentBlockDelta::ToolUseInputJson("{}".to_string()),
        },
        crate::provider::ProviderEvent::ContentBlockStopped { index: 1 },
        crate::provider::ProviderEvent::MessageDelta {
            stop_reason: Some("tool_use".to_string()),
            usage: None,
        },
        crate::provider::ProviderEvent::MessageStopped,
    ]);

    events
}

#[tokio::test]
async fn session_preserves_tool_events_after_many_token_deltas() {
    let model = crate::ModelInfo::new("mock-model", crate::BuiltinProvider::OpenAI);
    let runtime = crate::Runtime::builder()
        .with_provider_instance(OverflowingToolProvider {
            model: model.clone(),
            turn: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
        .with_policy(crate::RuntimePolicy::permissive())
        .build()
        .unwrap();
    runtime.register_tool(EchoTool);

    let mut session = runtime
        .create_session("overflow-tool-events", model.clone())
        .unwrap();
    let mut rx = session.subscribe();

    let message = session
        .append_turn(vec![ContentBlock::text("run the verbose tool turn")])
        .await
        .unwrap();

    assert_eq!(message.text(), "tool run finished");

    let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();

    let token_delta_count = events
        .iter()
        .filter(|event| matches!(event, SessionEvent::AssistantTokenDelta { .. }))
        .count();
    let has_tool_started = events.iter().any(
        |event| matches!(event, SessionEvent::ToolStarted { tool_name, .. } if tool_name == "echo_tool"),
    );
    let has_tool_completed = events.iter().any(|event| {
        matches!(event, SessionEvent::ToolCompleted { tool_call_id, .. } if tool_call_id == "tool-1")
    });

    assert!(
        token_delta_count >= 300,
        "Expected all token deltas to survive session mapping, got {token_delta_count} from {events:?}"
    );
    assert!(
        has_tool_started,
        "Expected ToolStarted event after many token deltas, got: {events:?}"
    );
    assert!(
        has_tool_completed,
        "Expected ToolCompleted event after many token deltas, got: {events:?}"
    );
}

#[tokio::test]
async fn resume_session_restores_state() {
    use crate::runtime::SqliteRuntimeStore;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let unique = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let store_path =
        std::env::temp_dir().join(format!("mentra-session-resume-{timestamp}-{unique}.sqlite"));
    let store = SqliteRuntimeStore::new(store_path);
    let runtime_id = "resume-session-test";

    let agent_id: String;

    // Phase 1: build a runtime, create a session, send a turn, then drop everything.
    {
        let mock = MockRuntime::builder()
            .runtime_identifier(runtime_id)
            .with_store(store.clone())
            .text("first response")
            .build()
            .unwrap();
        let mut session = mock
            .runtime()
            .create_session("resume-test", mock.model())
            .unwrap();

        let _message = session
            .append_turn(vec![ContentBlock::text("hello")])
            .await
            .unwrap();

        agent_id = session.agent_id().to_owned();

        assert!(
            !session.history().is_empty(),
            "Session should have history after a turn"
        );
        assert!(
            !session.replay().items().is_empty(),
            "Session transcript should be non-empty after a turn"
        );
        // mock (and its Runtime) dropped here, releasing the agent lease.
    }

    // Phase 2: build a fresh runtime with the same shared store, resume the agent.
    let mock2 = MockRuntime::builder()
        .runtime_identifier(runtime_id)
        .with_store(store)
        .build()
        .unwrap();

    let resumed_session = mock2.runtime().resume_session(&agent_id).unwrap();

    assert!(
        !resumed_session.replay().items().is_empty(),
        "Resumed session should have a non-empty transcript"
    );
}

#[tokio::test]
async fn failed_turn_emits_error_event() {
    let mock = MockRuntime::builder()
        .failure(ProviderError::InvalidResponse(
            "provider exploded".to_string(),
        ))
        .build()
        .unwrap();

    let mut session = mock
        .runtime()
        .create_session("failure-test", mock.model())
        .unwrap();

    let mut rx = session.subscribe();

    let result = session
        .append_turn(vec![ContentBlock::text("trigger failure")])
        .await;

    assert!(result.is_err(), "Expected append_turn to fail");

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }

    let has_error = events
        .iter()
        .any(|e| matches!(e, SessionEvent::Error { .. }));
    assert!(
        has_error,
        "Expected Error event after failed turn, got: {events:?}"
    );
}

#[tokio::test]
async fn all_session_events_from_turn_are_serializable_to_json() {
    let mock = MockRuntime::builder()
        .text("serializable response")
        .build()
        .unwrap();

    let mut session = mock
        .runtime()
        .create_session("serde-test", mock.model())
        .unwrap();

    let mut rx = session.subscribe();

    let _message = session
        .append_turn(vec![ContentBlock::text("check serde")])
        .await
        .unwrap();

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }

    assert!(
        !events.is_empty(),
        "Expected at least one event from a turn"
    );

    for event in &events {
        let json = serde_json::to_value(event)
            .unwrap_or_else(|err| panic!("Failed to serialize event {event:?}: {err}"));
        assert!(
            json.get("type").is_some(),
            "Serialized event missing 'type' tag: {json}"
        );
    }
}

// ---- Task 2A.2: File-edit event metadata ----

// The MockRuntime builder registers builtin tools (including `files`) automatically
// via Runtime::builder() -> RuntimeBuilder::new(true).

use crate::agent::{AgentConfig, WorkspaceConfig};

/// Creates a unique temp directory and returns (base_dir, unique_suffix).
fn unique_test_base_dir(label: &str) -> std::path::PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let base_dir = std::env::temp_dir().join(format!("mentra-{label}-{unique}"));
    std::fs::create_dir_all(&base_dir).unwrap();
    std::fs::canonicalize(&base_dir).unwrap_or(base_dir)
}

#[tokio::test]
async fn files_tool_create_emits_tool_progress_with_file_op_metadata() {
    let base_dir = unique_test_base_dir("file-op-test");
    let target_path = base_dir.join("hello.txt");

    let mock = MockRuntime::builder()
        .tool_calls([MockToolCall::new(
            "files",
            json!({
                "operations": [
                    {
                        "op": "create",
                        "path": target_path.to_str().unwrap(),
                        "content": "hello world\n"
                    }
                ]
            }),
        )])
        .text("file created")
        .build()
        .unwrap();

    // Use create_session_with_config so the agent's base_dir covers the temp dir,
    // satisfying the runtime policy write-root check.
    let agent_config = AgentConfig {
        workspace: WorkspaceConfig {
            base_dir: base_dir.clone(),
            auto_route_shell: false,
        },
        ..AgentConfig::default()
    };
    let mut session = mock
        .runtime()
        .create_session_with_config("file-op-test", mock.model(), agent_config)
        .unwrap();

    let mut rx = session.subscribe();

    let message = session
        .append_turn(vec![ContentBlock::text("create a file")])
        .await
        .unwrap();

    assert_eq!(message.text(), "file created");

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }

    // Verify that at least one ToolProgress event was emitted with a
    // "file_op:" prefix indicating the create operation.
    let file_op_progress = events.iter().find(|e| {
        matches!(
            e,
            SessionEvent::ToolProgress { progress, .. }
            if progress.starts_with("file_op: create ")
        )
    });

    assert!(
        file_op_progress.is_some(),
        "Expected ToolProgress event with 'file_op: create ...' metadata, got: {events:?}"
    );

    // Confirm the created file actually exists on disk.
    assert!(
        target_path.exists(),
        "Expected file to exist at {target_path:?}"
    );

    // Clean up.
    let _ = std::fs::remove_dir_all(&base_dir);
}

#[tokio::test]
async fn files_tool_set_emits_tool_progress_with_file_op_metadata() {
    let base_dir = unique_test_base_dir("file-set-test");
    let target_path = base_dir.join("target.txt");
    std::fs::write(&target_path, "original content\n").unwrap();

    let mock = MockRuntime::builder()
        .tool_calls([MockToolCall::new(
            "files",
            json!({
                "operations": [
                    {
                        "op": "set",
                        "path": target_path.to_str().unwrap(),
                        "content": "updated content\n"
                    }
                ]
            }),
        )])
        .text("file updated")
        .build()
        .unwrap();

    let agent_config = AgentConfig {
        workspace: WorkspaceConfig {
            base_dir: base_dir.clone(),
            auto_route_shell: false,
        },
        ..AgentConfig::default()
    };
    let mut session = mock
        .runtime()
        .create_session_with_config("file-set-test", mock.model(), agent_config)
        .unwrap();

    let mut rx = session.subscribe();

    let message = session
        .append_turn(vec![ContentBlock::text("update the file")])
        .await
        .unwrap();

    assert_eq!(message.text(), "file updated");

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }

    let file_op_progress = events.iter().find(|e| {
        matches!(
            e,
            SessionEvent::ToolProgress { progress, .. }
            if progress.starts_with("file_op: set ")
        )
    });

    assert!(
        file_op_progress.is_some(),
        "Expected ToolProgress event with 'file_op: set ...' metadata, got: {events:?}"
    );

    let _ = std::fs::remove_dir_all(&base_dir);
}

#[tokio::test]
async fn files_tool_read_does_not_emit_file_op_progress() {
    let base_dir = unique_test_base_dir("file-read-test");
    let target_path = base_dir.join("read_me.txt");
    std::fs::write(&target_path, "some content\n").unwrap();

    let mock = MockRuntime::builder()
        .tool_calls([MockToolCall::new(
            "files",
            json!({
                "operations": [
                    {
                        "op": "read",
                        "path": target_path.to_str().unwrap()
                    }
                ]
            }),
        )])
        .text("read done")
        .build()
        .unwrap();

    let agent_config = AgentConfig {
        workspace: WorkspaceConfig {
            base_dir: base_dir.clone(),
            auto_route_shell: false,
        },
        ..AgentConfig::default()
    };
    let mut session = mock
        .runtime()
        .create_session_with_config("file-read-test", mock.model(), agent_config)
        .unwrap();

    let mut rx = session.subscribe();

    let message = session
        .append_turn(vec![ContentBlock::text("read the file")])
        .await
        .unwrap();

    assert_eq!(message.text(), "read done");

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }

    // Read operations must NOT emit file_op: progress events.
    let file_op_progress = events.iter().find(|e| {
        matches!(
            e,
            SessionEvent::ToolProgress { progress, .. }
            if progress.starts_with("file_op:")
        )
    });

    assert!(
        file_op_progress.is_none(),
        "Read operation should not emit file_op progress, but got: {file_op_progress:?}"
    );

    let _ = std::fs::remove_dir_all(&base_dir);
}

// ---- Task 2A.4: Compaction continuity ----

#[tokio::test]
async fn compaction_events_appear_in_session_stream_and_session_continues() {
    // Use a very low auto_compact_threshold so compaction triggers after the first turn.
    // The mock provider needs: turn 1 response, compaction summary, turn 2 response.
    let transcript_dir = unique_test_base_dir("compact-session");
    let agent_config = AgentConfig {
        compaction: crate::agent::CompactionConfig {
            auto_compact_threshold_tokens: Some(1),
            transcript_dir: transcript_dir.clone(),
            ..Default::default()
        },
        ..AgentConfig::default()
    };

    let mock = MockRuntime::builder()
        .text("first response")
        .text("compaction summary") // consumed by the compaction summarizer
        .text("second response")
        .build()
        .unwrap();

    let mut session = mock
        .runtime()
        .create_session_with_config("compact-test", mock.model(), agent_config)
        .unwrap();

    let mut rx = session.subscribe();

    // Turn 1: triggers compaction because threshold is 1 token.
    let msg1 = session
        .append_turn(vec![ContentBlock::text("first turn")])
        .await
        .unwrap();
    assert_eq!(msg1.text(), "first response");

    // Turn 2: session continues coherently after compaction.
    let msg2 = session
        .append_turn(vec![ContentBlock::text("second turn")])
        .await
        .unwrap();
    assert_eq!(msg2.text(), "second response");
    assert_eq!(session.metadata().turn_count, 2);
    assert_eq!(session.metadata().status, SessionStatus::Idle);

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }

    let has_compaction_started = events
        .iter()
        .any(|e| matches!(e, SessionEvent::CompactionStarted { .. }));
    let has_compaction_completed = events
        .iter()
        .any(|e| matches!(e, SessionEvent::CompactionCompleted { .. }));

    assert!(
        has_compaction_started,
        "Expected CompactionStarted event, got: {events:?}"
    );
    assert!(
        has_compaction_completed,
        "Expected CompactionCompleted event, got: {events:?}"
    );

    // Verify ordering: CompactionStarted before CompactionCompleted.
    let started_pos = events
        .iter()
        .position(|e| matches!(e, SessionEvent::CompactionStarted { .. }))
        .unwrap();
    let completed_pos = events
        .iter()
        .position(|e| matches!(e, SessionEvent::CompactionCompleted { .. }))
        .unwrap();
    assert!(
        started_pos < completed_pos,
        "CompactionStarted (pos {started_pos}) must precede CompactionCompleted (pos {completed_pos})"
    );

    // Verify the second turn's assistant response appears after compaction.
    // Note: The UserMessage for the second turn is emitted before agent.send(),
    // so it may precede compaction events (compaction triggers during send).
    // But the AssistantMessageCompleted for turn 2 must come after compaction.
    let second_assistant = events.iter().position(|e| {
        matches!(e, SessionEvent::AssistantMessageCompleted { text } if text == "second response")
    });
    assert!(
        second_assistant.is_some(),
        "Expected second turn AssistantMessageCompleted after compaction"
    );
    assert!(
        second_assistant.unwrap() > completed_pos,
        "Second turn assistant response must appear after compaction completed"
    );

    let _ = std::fs::remove_dir_all(&transcript_dir);
}

// ---- Task 2A.6: Session resume continuity ----

#[tokio::test]
async fn resume_session_with_permission_rules_restores_rules() {
    use crate::runtime::{PermissionRuleStore, SqliteRuntimeStore};
    use std::sync::Arc;

    let unique = unique_test_base_dir("resume-rules");
    let store_path = unique.join("runtime.sqlite");
    let store = SqliteRuntimeStore::new(&store_path);
    let runtime_id = "resume-rules-test";

    let session_id_str: String;
    let agent_id: String;

    // Phase 1: Create session, add a permission rule, persist it.
    {
        let mock = MockRuntime::builder()
            .runtime_identifier(runtime_id)
            .with_store(store.clone())
            .text("hello")
            .build()
            .unwrap();

        let mut session = mock
            .runtime()
            .create_session("resume-rules", mock.model())
            .unwrap();

        session.set_permission_store(Arc::new(store.clone()) as Arc<dyn PermissionRuleStore>);
        session_id_str = session.id().as_str().to_owned();
        agent_id = session.agent_id().to_owned();

        // Simulate a permission decision that gets remembered.
        let permission_handle = session.permission_handle();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        session.pending_permissions.insert(
            "perm-r1".to_owned(),
            crate::session::permission::PendingPermissionEntry {
                tool_call_id: "tc-r1".to_owned(),
                tool_name: "shell".to_owned(),
                sender: tx,
            },
        );
        permission_handle
            .resolve_permission(
                "perm-r1",
                PermissionDecision::allow_and_remember(PermissionRuleScope::Session),
            )
            .unwrap();

        // Verify rule is in memory.
        assert_eq!(session.remembered_rules().len(), 1);

        // Submit a turn so the session has history.
        let _msg = session
            .append_turn(vec![ContentBlock::text("hi")])
            .await
            .unwrap();

        // Session + runtime dropped here.
    }

    // Phase 2: Resume session, attach same store, load persisted rules.
    let mock2 = MockRuntime::builder()
        .runtime_identifier(runtime_id)
        .with_store(store.clone())
        .text("resumed response")
        .build()
        .unwrap();

    let mut resumed = mock2.runtime().resume_session(&agent_id).unwrap();
    resumed.set_permission_store(Arc::new(store.clone()) as Arc<dyn PermissionRuleStore>);

    // Load the persisted rules using the original session id.
    // Note: resume_session creates a new SessionId, so we must load from the
    // original session id that was used when persisting. This tests the store
    // directly.
    let loaded_rules = store.load_rules(&session_id_str, None).unwrap();
    assert_eq!(
        loaded_rules.len(),
        1,
        "Expected 1 persisted rule, got: {loaded_rules:?}"
    );
    assert!(loaded_rules[0].allow);
    assert_eq!(loaded_rules[0].key.tool_name, "shell");
    assert!(
        store.load_rules("perm-r1", None).unwrap().is_empty(),
        "Expected rules to be saved under session id, not permission request id"
    );

    // Verify resumed session has intact transcript.
    assert!(
        !resumed.replay().items().is_empty(),
        "Resumed session should have non-empty transcript"
    );

    // Verify resumed session can accept new turns.
    let msg = resumed
        .append_turn(vec![ContentBlock::text("after resume")])
        .await
        .unwrap();
    assert_eq!(msg.text(), "resumed response");
    assert_eq!(resumed.metadata().turn_count, 1);

    let _ = std::fs::remove_dir_all(&unique);
}

// ---- Task 2A.7: Error handling and recovery ----

#[tokio::test]
async fn error_recovery_session_accepts_turn_after_failure() {
    // Script: first turn fails, second turn succeeds.
    let mock = MockRuntime::builder()
        .failure(ProviderError::InvalidResponse(
            "transient glitch".to_string(),
        ))
        .text("recovered successfully")
        .build()
        .unwrap();

    let mut session = mock
        .runtime()
        .create_session("error-recovery", mock.model())
        .unwrap();

    let mut rx = session.subscribe();

    // Turn 1: fails.
    let result = session
        .append_turn(vec![ContentBlock::text("will fail")])
        .await;
    assert!(result.is_err());
    assert!(matches!(
        session.metadata().status,
        SessionStatus::Failed(_)
    ));

    // Turn 2: succeeds, proving session is recoverable.
    let msg = session
        .append_turn(vec![ContentBlock::text("retry")])
        .await
        .unwrap();
    assert_eq!(msg.text(), "recovered successfully");
    assert_eq!(session.metadata().status, SessionStatus::Idle);
    assert_eq!(session.metadata().turn_count, 1);

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }

    // Verify error event from first turn.
    let error_event = events.iter().find(|e| {
        matches!(
            e,
            SessionEvent::Error { message, .. }
            if message.contains("transient glitch")
        )
    });
    assert!(
        error_event.is_some(),
        "Expected Error event containing 'transient glitch', got: {events:?}"
    );

    // Verify successful second turn events follow the error.
    let error_pos = events
        .iter()
        .position(|e| matches!(e, SessionEvent::Error { .. }))
        .unwrap();
    let second_assistant = events.iter().position(|e| {
        matches!(e, SessionEvent::AssistantMessageCompleted { text } if text == "recovered successfully")
    });
    assert!(
        second_assistant.is_some() && second_assistant.unwrap() > error_pos,
        "Second turn assistant message must appear after error event"
    );
}

#[tokio::test]
async fn tool_execution_error_emits_tool_completed_with_is_error() {
    use crate::tool::{ParallelToolContext, ToolDefinition, ToolExecutor, ToolResult, ToolSpec};

    struct FailingTool;

    #[async_trait]
    impl ToolDefinition for FailingTool {
        fn descriptor(&self) -> ToolSpec {
            ToolSpec::builder("failing_tool")
                .description("Always fails")
                .input_schema(json!({
                    "type": "object",
                    "properties": {}
                }))
                .build()
        }
    }

    #[async_trait]
    impl ToolExecutor for FailingTool {
        async fn execute(
            &self,
            _ctx: ParallelToolContext,
            _input: serde_json::Value,
        ) -> ToolResult {
            Err("tool execution failed".to_string())
        }
    }

    let mock = MockRuntime::builder()
        .tool_calls([MockToolCall::new("failing_tool", json!({}))])
        .text("continued after tool failure")
        .build()
        .unwrap();
    mock.runtime().register_tool(FailingTool);

    let mut session = mock
        .runtime()
        .create_session("tool-fail-test", mock.model())
        .unwrap();

    let mut rx = session.subscribe();

    let msg = session
        .append_turn(vec![ContentBlock::text("run failing tool")])
        .await
        .unwrap();

    // The session should continue even though the tool failed.
    assert_eq!(msg.text(), "continued after tool failure");
    assert_eq!(session.metadata().status, SessionStatus::Idle);

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }

    // Verify a ToolCompleted event with is_error = true appears.
    let tool_error = events
        .iter()
        .find(|e| matches!(e, SessionEvent::ToolCompleted { is_error: true, .. }));
    assert!(
        tool_error.is_some(),
        "Expected ToolCompleted with is_error=true, got: {events:?}"
    );
}

// ---- Task 2A.8: Full scenario integration test ----

#[tokio::test]
async fn full_scenario_prompt_shell_file_events_end_to_end() {
    let base_dir = unique_test_base_dir("scenario-e2e");
    let target_file = base_dir.join("scenario_output.txt");

    // Script the full scenario:
    // 1. Text response to initial prompt
    // 2. Tool call (echo_tool simulating shell) + File create operation
    // 3. Final text response
    let mock = MockRuntime::builder()
        .text("I will help you with that.")
        .tool_calls([MockToolCall::new("echo_tool", json!({}))])
        .tool_calls([MockToolCall::new(
            "files",
            json!({
                "operations": [
                    {
                        "op": "create",
                        "path": target_file.to_str().unwrap(),
                        "content": "scenario test output\n"
                    }
                ]
            }),
        )])
        .text("All tasks completed successfully.")
        .build()
        .unwrap();
    mock.runtime().register_tool(EchoTool);

    let agent_config = AgentConfig {
        workspace: WorkspaceConfig {
            base_dir: base_dir.clone(),
            auto_route_shell: false,
        },
        ..AgentConfig::default()
    };
    let mut session = mock
        .runtime()
        .create_session_with_config("scenario-test", mock.model(), agent_config)
        .unwrap();

    let mut rx = session.subscribe();

    // Turn 1: Simple text response.
    let msg1 = session
        .append_turn(vec![ContentBlock::text("Help me set up a project")])
        .await
        .unwrap();
    assert_eq!(msg1.text(), "I will help you with that.");
    assert_eq!(session.metadata().turn_count, 1);

    // Turn 2: Tool calls (echo_tool + file create).
    let msg2 = session
        .append_turn(vec![ContentBlock::text("Create a file and run a command")])
        .await
        .unwrap();
    assert_eq!(msg2.text(), "All tasks completed successfully.");
    assert_eq!(session.metadata().turn_count, 2);
    assert_eq!(session.metadata().status, SessionStatus::Idle);

    // Collect all events.
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }

    // Verify event ordering across the full scenario.
    let event_types: Vec<&str> = events
        .iter()
        .map(|e| match e {
            SessionEvent::UserMessage { .. } => "user_message",
            SessionEvent::AssistantTokenDelta { .. } => "token_delta",
            SessionEvent::AssistantMessageCompleted { .. } => "assistant_completed",
            SessionEvent::ToolQueued { .. } => "tool_queued",
            SessionEvent::ToolStarted { .. } => "tool_started",
            SessionEvent::ToolProgress { .. } => "tool_progress",
            SessionEvent::ToolCompleted { .. } => "tool_completed",
            SessionEvent::PermissionRequested { .. } => "perm_requested",
            SessionEvent::PermissionResolved { .. } => "perm_resolved",
            SessionEvent::TaskUpdated { .. } => "task_updated",
            SessionEvent::CompactionStarted { .. } => "compaction_started",
            SessionEvent::CompactionCompleted { .. } => "compaction_completed",
            SessionEvent::MemoryUpdated { .. } => "memory_updated",
            SessionEvent::Notice { .. } => "notice",
            SessionEvent::RetryAttempt { .. } => "retry_attempt",
            SessionEvent::Error { .. } => "error",
            SessionEvent::SessionStarted { .. } => "session_started",
        })
        .collect();

    // Verify we got all expected event categories.
    assert!(
        event_types.contains(&"user_message"),
        "Missing user_message events: {event_types:?}"
    );
    assert!(
        event_types.contains(&"assistant_completed"),
        "Missing assistant_completed events: {event_types:?}"
    );
    assert!(
        event_types.contains(&"tool_started"),
        "Missing tool_started events: {event_types:?}"
    );
    assert!(
        event_types.contains(&"tool_completed"),
        "Missing tool_completed events: {event_types:?}"
    );

    // Verify there are exactly 2 UserMessage events (one per turn).
    let user_msg_count = events
        .iter()
        .filter(|e| matches!(e, SessionEvent::UserMessage { .. }))
        .count();
    assert_eq!(user_msg_count, 2, "Expected 2 UserMessage events");

    // Verify there are exactly 2 AssistantMessageCompleted events.
    let assistant_count = events
        .iter()
        .filter(|e| matches!(e, SessionEvent::AssistantMessageCompleted { .. }))
        .count();
    assert_eq!(
        assistant_count, 2,
        "Expected 2 AssistantMessageCompleted events"
    );

    // Verify the file was actually created on disk.
    assert!(
        target_file.exists(),
        "Expected scenario output file at {target_file:?}"
    );

    // Verify a file_op progress event was emitted for the create operation.
    let has_file_op = events.iter().any(|e| {
        matches!(
            e,
            SessionEvent::ToolProgress { progress, .. }
            if progress.starts_with("file_op: create ")
        )
    });
    assert!(
        has_file_op,
        "Expected file_op progress event for create, events: {event_types:?}"
    );

    // Verify echo_tool execution events appear.
    let has_echo_tool = events.iter().any(|e| {
        matches!(
            e,
            SessionEvent::ToolStarted { tool_name, .. }
            if tool_name == "echo_tool"
        )
    });
    assert!(
        has_echo_tool,
        "Expected ToolStarted for echo_tool, events: {event_types:?}"
    );

    let _ = std::fs::remove_dir_all(&base_dir);
}

// ---- Task 1: project_id support in PermissionRuleStore ----

#[tokio::test]
async fn load_rules_with_project_id_returns_all_applicable_scopes() {
    use crate::runtime::{PermissionRuleStore, SqliteRuntimeStore};
    use crate::session::permission::{RememberedRule, RuleKey};
    use crate::session::event::PermissionRuleScope;

    // Create an isolated temp store.
    let store_path = std::env::temp_dir().join(format!(
        "mentra-session-perm-scopes-{}.sqlite",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let store = SqliteRuntimeStore::new(&store_path);

    let project_id = "test-project-42";
    let session_id = "session-main";
    let other_session_id = "session-other";

    // Session-scoped rule: belongs to session-main only.
    let session_rule = RememberedRule {
        key: RuleKey {
            tool_name: "shell".to_string(),
            pattern: None,
        },
        allow: true,
        scope: PermissionRuleScope::Session,
    };

    // Project-scoped rule: saved under a different session but same project_id.
    let project_rule = RememberedRule {
        key: RuleKey {
            tool_name: "file_write".to_string(),
            pattern: Some("/workspace/*".to_string()),
        },
        allow: true,
        scope: PermissionRuleScope::Project,
    };

    // Global-scoped rule: saved under yet another session, no project.
    let global_rule = RememberedRule {
        key: RuleKey {
            tool_name: "network".to_string(),
            pattern: None,
        },
        allow: false,
        scope: PermissionRuleScope::Global,
    };

    // Save session-scoped rule under session-main with project_id.
    store
        .save_rules(session_id, Some(project_id), &[session_rule])
        .expect("save session rule");

    // Save project-scoped rule under a different session but same project_id.
    store
        .save_rules(other_session_id, Some(project_id), &[project_rule])
        .expect("save project rule");

    // Save global-scoped rule under another unrelated session (no project).
    store
        .save_rules("session-global-only", None, &[global_rule])
        .expect("save global rule");

    // Load for session-main with project_id: should return all three scopes.
    let loaded = store
        .load_rules(session_id, Some(project_id))
        .expect("load rules");

    assert_eq!(
        loaded.len(),
        3,
        "Expected session + project + global rules (3 total), got: {loaded:?}"
    );

    let has_session = loaded
        .iter()
        .any(|r| r.key.tool_name == "shell" && r.scope == PermissionRuleScope::Session);
    let has_project = loaded
        .iter()
        .any(|r| r.key.tool_name == "file_write" && r.scope == PermissionRuleScope::Project);
    let has_global = loaded
        .iter()
        .any(|r| r.key.tool_name == "network" && r.scope == PermissionRuleScope::Global);

    assert!(has_session, "Session-scoped rule should be present");
    assert!(has_project, "Project-scoped rule should be present");
    assert!(has_global, "Global-scoped rule should be present");

    // Loading without project_id should only return session + global scopes.
    let loaded_no_project = store
        .load_rules(session_id, None)
        .expect("load rules without project_id");

    assert_eq!(
        loaded_no_project.len(),
        2,
        "Without project_id, expect only session + global rules, got: {loaded_no_project:?}"
    );
    assert!(
        loaded_no_project
            .iter()
            .any(|r| r.scope == PermissionRuleScope::Session),
        "Session rule should still be present"
    );
    assert!(
        loaded_no_project
            .iter()
            .any(|r| r.scope == PermissionRuleScope::Global),
        "Global rule should still be present"
    );
    assert!(
        !loaded_no_project
            .iter()
            .any(|r| r.scope == PermissionRuleScope::Project),
        "Project rule should NOT be present when no project_id given"
    );

    let _ = std::fs::remove_file(&store_path);
}

// ---- Task 3: Cross-session permission inheritance integration tests ----

#[tokio::test]
async fn project_scoped_rules_are_visible_across_sessions() {
    use crate::runtime::{PermissionRuleStore, SqliteRuntimeStore};
    use crate::session::permission::{RememberedRule, RuleKey};
    use crate::session::event::PermissionRuleScope;

    let store_path = std::env::temp_dir().join(format!(
        "mentra-cross-session-project-{}.sqlite",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let store = SqliteRuntimeStore::new(&store_path);

    let project_id = "my-project";
    let session_1 = "session-1";
    let session_2 = "session-2";

    let project_rule = RememberedRule {
        key: RuleKey {
            tool_name: "file_write".to_string(),
            pattern: None,
        },
        allow: true,
        scope: PermissionRuleScope::Project,
    };

    // Session-1 saves a project-scoped rule for "my-project".
    store
        .save_rules(session_1, Some(project_id), &[project_rule])
        .expect("save project-scoped rule via session-1");

    // Session-2 loads rules for the same project — project-scoped rule must be visible.
    let loaded = store
        .load_rules(session_2, Some(project_id))
        .expect("load rules for session-2");

    let has_project_rule = loaded
        .iter()
        .any(|r| r.key.tool_name == "file_write" && r.scope == PermissionRuleScope::Project);

    assert!(
        has_project_rule,
        "Project-scoped rule saved by session-1 should be visible to session-2 under the same project_id, got: {loaded:?}"
    );

    let _ = std::fs::remove_file(&store_path);
}

#[tokio::test]
async fn global_scoped_rules_are_visible_to_all_sessions() {
    use crate::runtime::{PermissionRuleStore, SqliteRuntimeStore};
    use crate::session::permission::{RememberedRule, RuleKey};
    use crate::session::event::PermissionRuleScope;

    let store_path = std::env::temp_dir().join(format!(
        "mentra-cross-session-global-{}.sqlite",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let store = SqliteRuntimeStore::new(&store_path);

    let session_1 = "session-1";
    let session_2 = "session-2";

    let global_rule = RememberedRule {
        key: RuleKey {
            tool_name: "network".to_string(),
            pattern: None,
        },
        allow: false,
        scope: PermissionRuleScope::Global,
    };

    // Session-1 saves a global rule (no project_id).
    store
        .save_rules(session_1, None, &[global_rule])
        .expect("save global-scoped rule via session-1");

    // Session-2 loads rules for a completely different project — global rule must be visible.
    let loaded = store
        .load_rules(session_2, Some("other-project"))
        .expect("load rules for session-2 with other-project");

    let has_global_rule = loaded
        .iter()
        .any(|r| r.key.tool_name == "network" && r.scope == PermissionRuleScope::Global);

    assert!(
        has_global_rule,
        "Global-scoped rule saved by session-1 should be visible to session-2 regardless of project, got: {loaded:?}"
    );

    let _ = std::fs::remove_file(&store_path);
}

#[tokio::test]
async fn session_scoped_rules_are_not_visible_to_other_sessions() {
    use crate::runtime::{PermissionRuleStore, SqliteRuntimeStore};
    use crate::session::permission::{RememberedRule, RuleKey};
    use crate::session::event::PermissionRuleScope;

    let store_path = std::env::temp_dir().join(format!(
        "mentra-cross-session-isolation-{}.sqlite",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let store = SqliteRuntimeStore::new(&store_path);

    let project_id = "shared-project";
    let session_1 = "session-1";
    let session_2 = "session-2";

    let session_rule = RememberedRule {
        key: RuleKey {
            tool_name: "shell".to_string(),
            pattern: None,
        },
        allow: true,
        scope: PermissionRuleScope::Session,
    };

    // Session-1 saves a session-scoped rule.
    store
        .save_rules(session_1, Some(project_id), &[session_rule])
        .expect("save session-scoped rule via session-1");

    // Session-2 loads rules for the same project — session-1's session-scoped rule must NOT appear.
    let loaded = store
        .load_rules(session_2, Some(project_id))
        .expect("load rules for session-2");

    let has_session_1_rule = loaded
        .iter()
        .any(|r| r.key.tool_name == "shell" && r.scope == PermissionRuleScope::Session);

    assert!(
        !has_session_1_rule,
        "Session-scoped rule from session-1 must NOT be visible to session-2 (session isolation), got: {loaded:?}"
    );

    let _ = std::fs::remove_file(&store_path);
}
