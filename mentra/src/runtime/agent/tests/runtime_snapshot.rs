use tokio::sync::watch;

use crate::{
    ContentBlock, ModelProviderKind, Role,
    provider::{ContentBlockDelta, ContentBlockStart, ProviderEvent},
    runtime::{AgentSnapshot, AgentStatus, BackgroundTaskStatus, Runtime},
};

use super::support::{ScriptedProvider, controlled_stream, model_info, ok_stream};

#[tokio::test]
async fn snapshot_progresses_during_streaming() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let (script, tx) = controlled_stream();
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
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
    let model = model_info("model", ModelProviderKind::Anthropic);
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
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
                    delta: ContentBlockDelta::ToolUseInputJson(
                        r#"{"command":"sleep 0.05; printf bg-done"}"#.to_string(),
                    ),
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

    wait_for_background_status(&mut snapshot, BackgroundTaskStatus::Running).await;
    wait_for_background_status(&mut snapshot, BackgroundTaskStatus::Finished).await;
    assert_eq!(snapshot.borrow().background_tasks.len(), 1);
    assert_eq!(
        snapshot.borrow().background_tasks[0]
            .output_preview
            .as_deref(),
        Some("bg-done")
    );
}

async fn wait_for_status(receiver: &mut watch::Receiver<AgentSnapshot>, status: AgentStatus) {
    loop {
        if receiver.borrow().status == status {
            return;
        }
        receiver.changed().await.unwrap();
    }
}

async fn wait_for_background_status(
    receiver: &mut watch::Receiver<AgentSnapshot>,
    status: BackgroundTaskStatus,
) {
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
}
