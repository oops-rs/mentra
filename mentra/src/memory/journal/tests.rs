use std::{collections::BTreeMap, sync::Arc};

use serde_json::json;

use crate::{
    AgentTranscript, ContentBlock, Message, Role, TranscriptKind,
    memory::journal::{AgentMemory, AgentMemoryState, CompactionOutcome, PendingTurnState},
    runtime::VolatileRuntimeStore,
};

#[test]
fn begin_run_commit_and_finish_persist_memory_state() {
    let store = Arc::new(VolatileRuntimeStore::new());
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
    let store = Arc::new(VolatileRuntimeStore::new());
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
    let store = Arc::new(VolatileRuntimeStore::new());
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

// ---- Crash recovery: a finished turn versus an interrupted one (#64) ----

fn memory() -> AgentMemory {
    AgentMemory::new(
        "agent-recovery",
        Arc::new(VolatileRuntimeStore::new()),
        AgentMemoryState::default(),
    )
}

fn tool_use(id: &str) -> ContentBlock {
    ContentBlock::ToolUse {
        id: id.to_string(),
        name: "write_file".to_string(),
        input: json!({}),
    }
}

fn tool_result(id: &str, content: &str) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: id.to_string(),
        content: content.to_string().into(),
        is_error: false,
    }
}

/// The `(tool_use_id, is_error, text)` of every tool result in `message`.
fn results_of(message: &Message) -> Vec<(String, bool, String)> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some((tool_use_id.clone(), *is_error, content.to_string())),
            _ => None,
        })
        .collect()
}

#[test]
fn a_crash_before_any_assistant_commit_rolls_the_turn_back() {
    let mut memory = memory();
    memory
        .begin_run("run-1".to_string(), Message::user(ContentBlock::text("hi")))
        .expect("begin run");
    memory
        .update_pending_turn(PendingTurnState::new("Hel".to_string(), Vec::new()))
        .expect("pending");

    let outcome = memory.recover().expect("recover");

    assert!(outcome.interrupted);
    assert!(memory.transcript().is_empty(), "nothing was committed");
    assert!(
        !memory.has_interrupted_turn(),
        "there is nothing to continue"
    );
    assert_eq!(
        memory.resumable_user_message(),
        Some(&Message::user(ContentBlock::text("hi"))),
        "the prompt is replayed instead"
    );
    assert!(memory.state().run.is_none());
}

#[test]
fn a_crash_while_tool_calls_ran_closes_each_call_and_keeps_the_turn_continuable() {
    let mut memory = memory();
    memory
        .begin_run("run-1".to_string(), Message::user(ContentBlock::text("go")))
        .expect("begin run");
    memory
        .commit_assistant_message(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::text("on it"), tool_use("a"), tool_use("b")],
        })
        .expect("commit tool calls");

    let outcome = memory.recover().expect("recover");

    assert!(outcome.interrupted);
    assert!(memory.has_interrupted_turn());
    assert!(
        memory.resumable_user_message().is_none(),
        "the prompt is in the transcript; replaying it would ask twice"
    );
    assert!(memory.state().run.is_none());
    let history = memory.history();
    assert_eq!(history.len(), 3, "prompt, tool calls, closed results");
    let closing = &history[2];
    assert_eq!(closing.role, Role::User);
    let results = results_of(closing);
    assert_eq!(
        results
            .iter()
            .map(|(id, ..)| id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"],
        "every unanswered call is closed, in call order"
    );
    for (_, is_error, text) in &results {
        assert!(*is_error, "a call whose effect is unknown is not a success");
        assert!(text.contains("stopped"), "{text}");
        assert!(text.contains("effect is unknown"), "{text}");
        assert!(text.contains("before repeating"), "{text}");
    }
}

#[test]
fn a_crash_after_tool_results_but_before_the_answer_keeps_the_results() {
    let mut memory = memory();
    memory
        .begin_run("run-1".to_string(), Message::user(ContentBlock::text("go")))
        .expect("begin run");
    memory
        .commit_assistant_message(Message::assistant(tool_use("a")))
        .expect("commit tool call");
    memory
        .append_message(Message::user(tool_result("a", "wrote it")))
        .expect("tool result");
    memory
        .update_pending_turn(PendingTurnState::new("Done".to_string(), Vec::new()))
        .expect("the answer was streaming");

    memory.recover().expect("recover");

    assert!(memory.has_interrupted_turn());
    assert!(memory.resumable_user_message().is_none());
    assert!(
        memory.state().pending_turn.is_none(),
        "the partial answer is gone"
    );
    let history = memory.history();
    assert_eq!(history.len(), 3, "nothing to close: the call was answered");
    assert_eq!(
        results_of(&history[2]),
        [("a".to_string(), false, "wrote it".to_string())],
        "the real result stands; the call is never run again"
    );
}

#[test]
fn a_crash_after_the_final_answer_was_committed_is_a_finished_turn() {
    let mut memory = memory();
    memory
        .begin_run("run-1".to_string(), Message::user(ContentBlock::text("go")))
        .expect("begin run");
    memory
        .commit_assistant_message(Message::assistant(tool_use("a")))
        .expect("commit tool call");
    memory
        .append_message(Message::user(tool_result("a", "wrote it")))
        .expect("tool result");
    memory
        .commit_assistant_message(Message::assistant(ContentBlock::text("done")))
        .expect("final answer");

    memory.recover().expect("recover");

    assert!(!memory.has_interrupted_turn());
    assert!(memory.resumable_user_message().is_none());
    assert_eq!(memory.history().len(), 4, "the transcript is kept as is");
    assert_eq!(
        memory.last_message(),
        Some(&Message::assistant(ContentBlock::text("done")))
    );
}

#[test]
fn a_continuation_that_fails_or_crashes_before_committing_stays_continuable() {
    let mut memory = memory();
    memory
        .begin_run("run-1".to_string(), Message::user(ContentBlock::text("go")))
        .expect("begin run");
    memory
        .commit_assistant_message(Message::assistant(tool_use("a")))
        .expect("commit tool call");
    memory.recover().expect("recover");
    let recovered = memory.transcript().clone();

    memory
        .begin_continuation("run-2".to_string())
        .expect("continue");
    memory
        .update_pending_turn(PendingTurnState::new("par".to_string(), Vec::new()))
        .expect("pending");
    memory.rollback_failed_run().expect("rollback");
    assert_eq!(memory.transcript(), &recovered);
    assert!(
        memory.has_interrupted_turn(),
        "a failed continuation can be retried"
    );
    assert!(memory.resumable_user_message().is_none());

    memory
        .begin_continuation("run-3".to_string())
        .expect("continue again");
    memory.recover().expect("recover the crashed continuation");
    assert_eq!(memory.transcript(), &recovered);
    assert!(memory.has_interrupted_turn());
}

#[test]
fn a_finished_continuation_ends_the_interrupted_turn() {
    let mut memory = memory();
    memory
        .begin_run("run-1".to_string(), Message::user(ContentBlock::text("go")))
        .expect("begin run");
    memory
        .commit_assistant_message(Message::assistant(tool_use("a")))
        .expect("commit tool call");
    memory.recover().expect("recover");

    memory
        .begin_continuation("run-2".to_string())
        .expect("continue");
    memory
        .commit_assistant_message(Message::assistant(ContentBlock::text("checked; done")))
        .expect("answer");
    assert_eq!(
        memory.current_run_delta(),
        Some(vec![Message::assistant(ContentBlock::text(
            "checked; done"
        ))]),
        "a continuation adds no user message of its own"
    );
    memory.finish_run().expect("finish");

    assert!(!memory.has_interrupted_turn());
    assert_eq!(memory.history().len(), 4);
}

#[test]
fn a_new_user_turn_does_not_forget_the_interrupted_one_if_it_fails() {
    let mut memory = memory();
    memory
        .begin_run("run-1".to_string(), Message::user(ContentBlock::text("go")))
        .expect("begin run");
    memory
        .commit_assistant_message(Message::assistant(tool_use("a")))
        .expect("commit tool call");
    memory.recover().expect("recover");

    memory
        .begin_run(
            "run-2".to_string(),
            Message::user(ContentBlock::text("something else")),
        )
        .expect("begin a new turn");
    memory.rollback_failed_run().expect("it failed");
    assert!(memory.has_interrupted_turn());

    memory
        .begin_run(
            "run-3".to_string(),
            Message::user(ContentBlock::text("something else")),
        )
        .expect("begin a new turn");
    memory
        .commit_assistant_message(Message::assistant(ContentBlock::text("ok")))
        .expect("answer");
    memory.finish_run().expect("finish");
    assert!(
        !memory.has_interrupted_turn(),
        "a finished turn leaves the transcript finished"
    );
}

#[test]
fn state_recovered_by_an_older_version_is_repaired_on_load() {
    // What mentra before #64 left behind: the run cleared, the transcript
    // ending in tool calls nobody answered, and no resumable turn.
    let state = AgentMemoryState {
        transcript: AgentTranscript::from_messages(vec![
            Message::user(ContentBlock::text("go")),
            Message::assistant(tool_use("a")),
        ]),
        revision: 4,
        ..AgentMemoryState::default()
    };
    let mut memory = AgentMemory::new("agent-legacy", Arc::new(VolatileRuntimeStore::new()), state);

    let outcome = memory.recover().expect("recover");

    assert!(!outcome.interrupted, "no run was in progress");
    assert!(memory.has_interrupted_turn());
    let history = memory.history();
    assert_eq!(history.len(), 3);
    let results = results_of(&history[2]);
    assert_eq!(results.len(), 1);
    assert!(results[0].1, "closed as an error");
}

#[test]
fn memory_state_without_the_interrupted_marker_still_loads() {
    let legacy = json!({
        "transcript": [],
        "pending_turn": null,
        "resumable_user_message": null,
        "revision": 2,
        "run": {
            "run_id": "run-1",
            "baseline_transcript": [],
            "assistant_committed": true
        }
    });
    let state: AgentMemoryState = serde_json::from_value(legacy).expect("legacy state loads");
    assert!(!state.interrupted_turn);

    let written = serde_json::to_value(AgentMemoryState::default()).expect("serialize");
    assert!(
        written.get("interrupted_turn").is_none(),
        "the marker is written only when set, so older readers see what they always did"
    );
    let marked = AgentMemoryState {
        interrupted_turn: true,
        ..AgentMemoryState::default()
    };
    let round_trip: AgentMemoryState =
        serde_json::from_value(serde_json::to_value(&marked).expect("serialize"))
            .expect("deserialize");
    assert!(round_trip.interrupted_turn);
}
