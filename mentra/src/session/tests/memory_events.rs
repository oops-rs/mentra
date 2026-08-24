//! Tests for [`SessionEvent::MemoryUpdated`] reaching the session stream: the
//! hook bridge is registered on the per-session runtime handle, so one
//! session's memory activity lands on that session's channel and no other's.

use std::time::Duration;

use crate::{ContentBlock, session::event::SessionEvent, test::MockRuntime};

/// Waits until the stream yields a `MemoryUpdated`, skipping everything else.
///
/// Ingest is scheduled on a detached task after a run finishes, so the event
/// arrives asynchronously; the timeout turns a wiring regression into a fast
/// failure instead of a hung test.
async fn wait_for_memory_updated(rx: &mut crate::session::SessionEventReceiver) -> (String, usize) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Ok(SessionEvent::MemoryUpdated {
                    agent_id,
                    stored_records,
                }) => return (agent_id, stored_records),
                Ok(_) => continue,
                Err(error) => panic!("event stream closed before MemoryUpdated: {error}"),
            }
        }
    })
    .await
    .expect("timed out waiting for MemoryUpdated on the session stream")
}

#[tokio::test]
async fn a_turns_memory_ingest_reaches_the_session_stream() {
    let mock = MockRuntime::builder().text("done").build().unwrap();
    let mut session = mock
        .runtime()
        .create_session("memory-events", mock.model())
        .unwrap();

    let mut rx = session.subscribe();
    session
        .append_turn(vec![ContentBlock::text("remember this")])
        .await
        .unwrap();

    let (agent_id, stored_records) = wait_for_memory_updated(&mut rx).await;
    assert_eq!(
        agent_id,
        session.agent_id(),
        "the event must name the session's own agent"
    );
    assert_eq!(
        stored_records, 1,
        "a turn with content must have stored its episode"
    );
}

#[tokio::test]
async fn memory_updated_does_not_cross_between_sessions() {
    let mock = MockRuntime::builder().text("done").build().unwrap();
    let mut active = mock
        .runtime()
        .create_session("active", mock.model())
        .unwrap();
    let bystander = mock
        .runtime()
        .create_session("bystander", mock.model())
        .unwrap();

    let mut active_rx = active.subscribe();
    let mut bystander_rx = bystander.subscribe();

    active
        .append_turn(vec![ContentBlock::text("remember this")])
        .await
        .unwrap();

    // The active session's event arriving is the ordering point: a wrongly
    // shared bridge would have sent to both channels in the same call.
    let (agent_id, _) = wait_for_memory_updated(&mut active_rx).await;
    assert_eq!(agent_id, active.agent_id());

    assert!(
        bystander_rx.try_recv().is_err(),
        "another session on the same runtime must not see this agent's memory activity"
    );
}

#[tokio::test]
async fn a_resumed_session_still_emits_memory_updated() {
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
        std::env::temp_dir().join(format!("mentra-memory-resume-{timestamp}-{unique}.sqlite"));
    let store = SqliteRuntimeStore::new(store_path);
    let runtime_id = "memory-resume-test";

    let agent_id: String;

    // Phase 1: run one turn so there is a persisted agent to resume.
    {
        let mock = MockRuntime::builder()
            .runtime_identifier(runtime_id)
            .with_store(store.clone())
            .text("first response")
            .build()
            .unwrap();
        let mut session = mock
            .runtime()
            .create_session("memory-resume", mock.model())
            .unwrap();
        session
            .append_turn(vec![ContentBlock::text("hello")])
            .await
            .unwrap();
        agent_id = session.agent_id().to_owned();
        // mock (and its Runtime) dropped here, releasing the agent lease.
    }

    // Phase 2: resume on a fresh runtime; the resumed session's stream must
    // carry the memory events of its next turn.
    let mock2 = MockRuntime::builder()
        .runtime_identifier(runtime_id)
        .with_store(store)
        .text("second response")
        .build()
        .unwrap();
    let mut resumed = mock2.runtime().resume_session(&agent_id).unwrap();

    let mut rx = resumed.subscribe();
    resumed
        .append_turn(vec![ContentBlock::text("remember more")])
        .await
        .unwrap();

    let (event_agent, _) = wait_for_memory_updated(&mut rx).await;
    assert_eq!(
        event_agent, agent_id,
        "the resumed session must report its own agent's ingest"
    );
}
