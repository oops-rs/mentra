use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use tokio::{
    sync::watch,
    time::{Duration, timeout},
};

use crate::{
    AgentConfig, BackgroundTaskStatus, BuiltinProvider, ContentBlock, Role,
    agent::{AgentSnapshot, AgentStatus, TeamAutonomyConfig, TeamConfig},
    provider::{ContentBlockDelta, ContentBlockStart, ProviderEvent},
    runtime::{Runtime, RuntimePolicy, SqliteRuntimeStore},
};

use super::support::{
    ScriptedProvider, background_success_command, command_input_json, controlled_stream,
    model_info, ok_stream, text_stream,
};

#[tokio::test]
async fn owned_waits_coexist_with_mutable_runs_and_track_run_generation() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            text_stream(&model.id, "first"),
            text_stream(&model.id, "second"),
        ],
    );
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    let first_idle = agent.wait_until_idle();
    let first_finished = agent.wait_for_snapshot(|snapshot| {
        snapshot.run_generation == 1 && snapshot.status == AgentStatus::Finished
    });
    let (first_snapshot, predicate_snapshot, first_result) = tokio::join!(
        first_idle,
        first_finished,
        agent.send(vec![ContentBlock::text("first run")])
    );
    assert_eq!(first_result.expect("first run").text(), "first");
    assert_eq!(first_snapshot.run_generation, 1);
    assert_eq!(predicate_snapshot.run_generation, 1);
    assert_eq!(first_snapshot.status, AgentStatus::Finished);

    // Constructed while the previous generation is terminal: this must wait
    // for generation 2 rather than immediately returning generation 1.
    let second_idle = agent.wait_until_idle();
    let (second_snapshot, second_result) = tokio::join!(
        second_idle,
        agent.send(vec![ContentBlock::text("second run")])
    );
    assert_eq!(second_result.expect("second run").text(), "second");
    assert_eq!(second_snapshot.run_generation, 2);
    assert_eq!(second_snapshot.status, AgentStatus::Finished);
}

#[tokio::test]
async fn teammate_reply_wait_consumes_the_snapshot_signaled_inbox() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(BuiltinProvider::Anthropic, vec![model.clone()], vec![]);
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let team_dir = std::env::temp_dir().join(format!(
        "mentra-wait-team-{}-{}",
        std::process::id(),
        NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let config = AgentConfig {
        team: TeamConfig {
            team_dir,
            autonomy: TeamAutonomyConfig::default(),
        },
        ..AgentConfig::default()
    };
    let alice = runtime
        .spawn_with_config("alice", model.clone(), config.clone())
        .expect("spawn alice");
    let bob = runtime
        .spawn_with_config("bob", model, config)
        .expect("spawn bob");
    let reply = bob.wait_for_teammate_reply();

    alice
        .send_team_message("bob", "the review is ready")
        .expect("send reply");
    let messages = timeout(Duration::from_secs(5), reply)
        .await
        .expect("reply wait timed out")
        .expect("read reply");

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].sender, "alice");
    assert_eq!(messages[0].content, "the review is ready");
    assert_eq!(bob.watch_snapshot().borrow().pending_team_messages, 0);
}

#[tokio::test]
async fn snapshot_progresses_during_streaming() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let (script, tx) = controlled_stream();
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![script],
    );

    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let agent = runtime.spawn("agent", model.clone()).unwrap();
    let mut snapshot = agent.watch_snapshot();

    let send_task = tokio::spawn(async move {
        let mut agent = agent;
        let result = agent
            .send(vec![ContentBlock::Text {
                text: "hello".to_string(),
            }])
            .await;
        (agent, result)
    });

    wait_for_status(&mut snapshot, AgentStatus::Streaming).await;

    tx.send(Ok(ProviderEvent::MessageStarted {
        id: "msg-1".to_string(),
        model: model.id,
        role: Role::Assistant,
    }))
    .unwrap();
    snapshot.changed().await.unwrap();

    tx.send(Ok(ProviderEvent::ContentBlockStarted {
        index: 0,
        kind: ContentBlockStart::Text,
    }))
    .unwrap();
    snapshot.changed().await.unwrap();

    tx.send(Ok(ProviderEvent::ContentBlockDelta {
        index: 0,
        delta: ContentBlockDelta::Text("Hel".to_string()),
    }))
    .unwrap();
    snapshot.changed().await.unwrap();
    assert_eq!(snapshot.borrow().current_text, "Hel");

    tx.send(Ok(ProviderEvent::ContentBlockDelta {
        index: 0,
        delta: ContentBlockDelta::Text("lo".to_string()),
    }))
    .unwrap();
    snapshot.changed().await.unwrap();
    assert_eq!(snapshot.borrow().current_text, "Hello");

    tx.send(Ok(ProviderEvent::ContentBlockStopped { index: 0 }))
        .unwrap();
    tx.send(Ok(ProviderEvent::MessageStopped)).unwrap();
    drop(tx);

    let (agent, result) = send_task.await.unwrap();
    result.unwrap();

    let snapshot = agent.watch_snapshot();
    assert_eq!(snapshot.borrow().status, AgentStatus::Finished);
    assert!(snapshot.borrow().current_text.is_empty());
    assert!(snapshot.borrow().pending_tool_uses.is_empty());
}

#[tokio::test]
async fn snapshot_updates_when_background_task_finishes() {
    let command = background_success_command("bg-done", 50);
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            ok_stream(vec![
                ProviderEvent::MessageStarted {
                    id: "msg-bg".to_string(),
                    model: model.id.clone(),
                    role: Role::Assistant,
                },
                ProviderEvent::ContentBlockStarted {
                    index: 0,
                    kind: ContentBlockStart::ToolUse {
                        id: "tool-bg".to_string(),
                        name: "background_run".to_string(),
                    },
                },
                ProviderEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::ToolUseInputJson(command_input_json(&command)),
                },
                ProviderEvent::ContentBlockStopped { index: 0 },
                ProviderEvent::MessageStopped,
            ]),
            ok_stream(vec![
                ProviderEvent::MessageStarted {
                    id: "msg-follow".to_string(),
                    model: model.id.clone(),
                    role: Role::Assistant,
                },
                ProviderEvent::ContentBlockStarted {
                    index: 0,
                    kind: ContentBlockStart::Text,
                },
                ProviderEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::Text("continued".to_string()),
                },
                ProviderEvent::ContentBlockStopped { index: 0 },
                ProviderEvent::MessageStopped,
            ]),
        ],
    );

    let runtime = Runtime::builder()
        .with_store(temp_store("snapshot-background-finish"))
        .with_policy(RuntimePolicy::permissive())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();
    let mut snapshot = agent.watch_snapshot();

    agent
        .send(vec![ContentBlock::Text {
            text: "run background command".to_string(),
        }])
        .await
        .unwrap();

    wait_for_background_status(&mut snapshot, BackgroundTaskStatus::Finished).await;
    assert_eq!(snapshot.borrow().background_tasks.len(), 1);
    assert!(
        snapshot.borrow().background_tasks[0]
            .output_preview
            .as_deref()
            .is_some_and(|preview| preview.contains("bg-done"))
    );
}

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

fn temp_store(label: &str) -> SqliteRuntimeStore {
    let unique = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    SqliteRuntimeStore::new(std::env::temp_dir().join(format!(
        "mentra-runtime-store-{label}-{timestamp}-{unique}.sqlite"
    )))
}

async fn wait_for_status(receiver: &mut watch::Receiver<AgentSnapshot>, status: AgentStatus) {
    timeout(Duration::from_secs(90), async {
        loop {
            if receiver.borrow().status == status {
                return;
            }
            receiver.changed().await.unwrap();
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for agent status {status:?}"));
}

async fn wait_for_background_status(
    receiver: &mut watch::Receiver<AgentSnapshot>,
    status: BackgroundTaskStatus,
) {
    timeout(Duration::from_secs(90), async {
        loop {
            if receiver
                .borrow()
                .background_tasks
                .iter()
                .any(|task| task.status == status)
            {
                return;
            }
            receiver.changed().await.unwrap();
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for background status {status:?}"));
}
