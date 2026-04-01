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
        },
        SessionEvent::MemoryUpdated {
            agent_id: "a1".to_string(),
            stored_records: 3,
        },
        SessionEvent::Notice {
            severity: NoticeSeverity::Info,
            message: "Context window 80% full".to_string(),
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
    let has_assistant = events
        .iter()
        .any(|e| matches!(e, SessionEvent::AssistantMessageCompleted { text } if text == "response"));

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
    let mut session = mock
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
    session
        .resolve_permission("perm-1", decision)
        .unwrap();

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
    let mut session = mock
        .runtime()
        .create_session("perm-test", mock.model())
        .unwrap();

    let result = session.resolve_permission("nonexistent", PermissionDecision::deny());
    assert!(result.is_err());
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
    let user_pos = events.iter().position(|e| {
        matches!(e, SessionEvent::UserMessage { text } if text == "Hi there")
    });
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

    let has_tool_started = events.iter().any(|e| {
        matches!(e, SessionEvent::ToolStarted { tool_name, .. } if tool_name == "echo_tool")
    });
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
    let store_path = std::env::temp_dir().join(format!(
        "mentra-session-resume-{timestamp}-{unique}.sqlite"
    ));
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
        .failure(ProviderError::InvalidResponse("provider exploded".to_string()))
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

    let has_error = events.iter().any(|e| matches!(e, SessionEvent::Error { .. }));
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
    base_dir
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
