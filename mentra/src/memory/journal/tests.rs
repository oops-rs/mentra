use std::{collections::BTreeMap, sync::Arc};

use serde_json::json;

use crate::{
    AgentTranscript, ContentBlock, Message, TranscriptKind,
    memory::journal::{AgentMemory, AgentMemoryState, CompactionOutcome, PendingTurnState},
    runtime::SqliteRuntimeStore,
};

#[test]
fn begin_run_commit_and_finish_persist_memory_state() {
    let store = Arc::new(SqliteRuntimeStore::new(std::env::temp_dir().join(format!(
        "mentra-memory-test-{}.sqlite",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ))));
    let mut memory = AgentMemory::new("agent-test", store, AgentMemoryState::default());

    memory
        .begin_run(
            "run-1".to_string(),
            Message::user(ContentBlock::text("hello")),
        )
        .expect("begin run");
    assert_eq!(memory.transcript().len(), 1);
    assert_eq!(memory.state().revision, 1);
    assert_eq!(
        memory.resumable_user_message(),
        Some(&Message::user(ContentBlock::text("hello")))
    );

    memory
        .update_pending_turn(PendingTurnState::new("Hel".to_string(), Vec::new()))
        .expect("update pending");
    assert_eq!(memory.snapshot_view().current_text, "Hel");

    memory
        .commit_assistant_message(Message::assistant(ContentBlock::text("done")))
        .expect("commit message");
    assert_eq!(memory.transcript().len(), 2);
    assert!(memory.snapshot_view().current_text.is_empty());

    memory.finish_run().expect("finish run");
    assert!(memory.state().run.is_none());
    assert!(memory.resumable_user_message().is_none());
}

#[test]
fn rollback_and_compaction_update_memory_state() {
    let store = Arc::new(SqliteRuntimeStore::new(std::env::temp_dir().join(format!(
        "mentra-memory-rollback-test-{}.sqlite",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ))));
    let mut memory = AgentMemory::new("agent-test", store, AgentMemoryState::default());

    memory
        .begin_run(
            "run-1".to_string(),
            Message::user(ContentBlock::text("hello")),
        )
        .expect("begin run");
    memory
        .update_pending_turn(PendingTurnState::new("partial".to_string(), Vec::new()))
        .expect("pending");
    memory.rollback_failed_run().expect("rollback");
    assert!(memory.transcript().is_empty());
    assert_eq!(
        memory.resumable_user_message(),
        Some(&Message::user(ContentBlock::text("hello")))
    );

    memory
        .append_message(Message::user(ContentBlock::text("after")))
        .expect("append");
    let path = std::env::temp_dir().join("compacted.jsonl");
    memory
        .compact(CompactionOutcome {
            transcript_path: path.clone(),
            transcript: AgentTranscript::from_messages(vec![Message::user(ContentBlock::text(
                "summary",
            ))]),
        })
        .expect("compact");
    assert_eq!(memory.transcript().len(), 1);
    let _ = path;
}

// M3: `append_message_with_details` is an additive counterpart to
// `append_message` that behaves identically except for attaching metadata —
// proven directly against the in-process transcript here. The full
// persist/reload round-trip through the SQLite store (which additionally
// requires a real `agents` row, written by `Runtime::spawn`/`create_agent`,
// not just `AgentMemory` in isolation) is covered end-to-end in
// `agent::tests::runtime_resume::resumed_agent_keeps_tool_result_details_after_restart`.
#[test]
fn append_message_with_details_attaches_metadata_keyed_by_tool_use_id() {
    let store = Arc::new(SqliteRuntimeStore::new(std::env::temp_dir().join(format!(
        "mentra-memory-details-test-{}.sqlite",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ))));
    let mut memory = AgentMemory::new("agent-details", store, AgentMemoryState::default());

    memory
        .begin_run(
            "run-1".to_string(),
            Message::user(ContentBlock::text("run the details tool")),
        )
        .expect("begin run");
    memory
        .commit_assistant_message(Message::assistant(ContentBlock::ToolUse {
            id: "call-1".to_string(),
            name: "details_tool".to_string(),
            input: json!({}),
        }))
        .expect("commit assistant tool call");

    let details: BTreeMap<String, serde_json::Value> =
        BTreeMap::from([("call-1".to_string(), json!({ "secret": "shh", "n": 42 }))]);
    memory
        .append_message_with_details(
            Message::user(ContentBlock::ToolResult {
                tool_use_id: "call-1".to_string(),
                content: "tool output".to_string().into(),
                is_error: false,
            }),
            details.clone(),
        )
        .expect("append with details");

    let item = memory
        .transcript()
        .items()
        .iter()
        .find(|item| matches!(item.kind, TranscriptKind::ToolExchange { .. }))
        .expect("transcript keeps the tool exchange item");
    assert_eq!(item.details(), Some(&details));
    assert_eq!(
        item.detail("call-1"),
        Some(&json!({ "secret": "shh", "n": 42 }))
    );
}
