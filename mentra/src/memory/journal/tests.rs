use std::sync::Arc;

use crate::{
    ContentBlock, Message,
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
            transcript: vec![Message::user(ContentBlock::text("summary"))],
        })
        .expect("compact");
    assert_eq!(memory.transcript().len(), 1);
    assert_eq!(
        memory.state().compaction.last_compacted_transcript_path,
        Some(path)
    );
}
