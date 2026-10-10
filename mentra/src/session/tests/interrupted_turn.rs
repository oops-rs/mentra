//! A session resumed over a turn a crash interrupted mid tool round (#64).

use serde_json::json;

use crate::{
    ContentBlock, Message,
    memory::journal::RunMemoryState,
    runtime::{AgentStore, RunOptions, VolatileRuntimeStore},
    session::{SessionEvent, SessionStatus},
    test::MockRuntime,
};

/// Leaves `agent_id`'s memory as a process that died while a tool call ran
/// would have: a run open, and an assistant message whose call has no result.
fn crash_mid_tool_round(store: &VolatileRuntimeStore, agent_id: &str) {
    let mut memory = store.load_agent(agent_id).unwrap().unwrap().memory;
    let baseline = memory.transcript.clone();
    for message in [
        Message::user(ContentBlock::text("write the file")),
        Message::assistant(ContentBlock::ToolUse {
            id: "call-1".to_string(),
            name: "write_file".to_string(),
            input: json!({ "path": "out.txt" }),
        }),
    ] {
        memory
            .transcript
            .push(crate::transcript::transcript_item_from_message(message));
    }
    memory.run = Some(RunMemoryState {
        run_id: "run-crashed".to_string(),
        baseline_transcript: baseline,
        assistant_committed: true,
    });
    store.save_agent_memory(agent_id, &memory).unwrap();
}

#[tokio::test]
async fn a_resumed_session_continues_an_interrupted_turn_without_a_user_message() {
    let store = VolatileRuntimeStore::new();
    let agent_id = {
        let mock = MockRuntime::builder()
            .with_store(store.clone())
            .build()
            .unwrap();
        let session = mock
            .runtime()
            .create_session("interrupted", mock.model())
            .unwrap();
        session.agent_id().to_string()
    };
    crash_mid_tool_round(&store, &agent_id);

    let mock = MockRuntime::builder()
        .with_store(store)
        .text("checked the file; it is written")
        .build()
        .unwrap();
    let mut session = mock.runtime().resume_session(&agent_id).unwrap();
    assert!(session.has_interrupted_turn());
    assert_eq!(session.history().len(), 3, "prompt, call, closed result");

    let mut events = session.subscribe();
    let answer = session
        .continue_interrupted(RunOptions::default())
        .await
        .unwrap();

    assert_eq!(answer.text(), "checked the file; it is written");
    assert!(!session.has_interrupted_turn());
    assert_eq!(session.history().len(), 4);
    assert_eq!(session.metadata().status, SessionStatus::Idle);
    let mut seen = Vec::new();
    while let Ok(event) = events.try_recv() {
        seen.push(event);
    }
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, SessionEvent::UserMessage { .. })),
        "nobody said anything new: {seen:?}"
    );
    assert!(seen.iter().any(|event| matches!(
        event,
        SessionEvent::AssistantMessageCompleted { text } if text == "checked the file; it is written"
    )));

    let again = session.continue_interrupted(RunOptions::default()).await;
    assert!(matches!(
        again,
        Err(crate::runtime::RuntimeError::NoInterruptedTurn)
    ));
}
