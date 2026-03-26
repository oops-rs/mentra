use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use tokio::{
    sync::watch,
    time::{Duration, timeout},
};

use crate::{
    BackgroundTaskStatus, BuiltinProvider, ContentBlock, Role,
    agent::{AgentSnapshot, AgentStatus},
    provider::{ContentBlockDelta, ContentBlockStart, ProviderEvent},
    runtime::{Runtime, RuntimePolicy, SqliteRuntimeStore},
};

use super::support::{
    ScriptedProvider, background_success_command, command_input_json, controlled_stream,
    model_info, ok_stream,
};

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
    timeout(Duration::from_secs(20), async {
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
    timeout(Duration::from_secs(20), async {
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
