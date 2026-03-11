use crate::{
    provider::{ContentBlockDelta, ContentBlockStart, ProviderError, ProviderEvent},
    ContentBlock, Message, ModelProviderKind, Role,
    runtime::{AgentConfig, AgentEvent, AgentStatus, Runtime},
};

use super::support::{ScriptedProvider, erroring_stream, model_info, ok_stream};

#[tokio::test]
async fn send_streamed_text_turn_emits_events_and_commits_history() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![ok_stream(vec![
            ProviderEvent::MessageStarted {
                id: "msg-1".to_string(),
                model: model.id.clone(),
                role: Role::Assistant,
            },
            ProviderEvent::ContentBlockStarted {
                index: 0,
                kind: ContentBlockStart::Text,
            },
            ProviderEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text("Hello".to_string()),
            },
            ProviderEvent::ContentBlockStopped { index: 0 },
            ProviderEvent::MessageStopped,
        ])],
    );

    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                system: Some("system prompt".to_string()),
                ..AgentConfig::default()
            },
        )
        .unwrap();
    let mut events = agent.subscribe_events();

    agent
        .send(vec![ContentBlock::Text {
            text: "hi".to_string(),
        }])
        .await
        .unwrap();

    assert_eq!(agent.name(), "agent");
    assert_eq!(agent.model(), "model");
    assert_eq!(agent.history().len(), 2);
    assert_eq!(agent.config().system.as_deref(), Some("system prompt"));
    assert_eq!(
        agent.last_message(),
        Some(&Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "Hello".to_string(),
            }],
        })
    );

    let events = collect_events(&mut events);
    assert!(events.contains(&AgentEvent::RunStarted));
    assert!(events.contains(&AgentEvent::TextDelta {
        delta: "Hello".to_string(),
        full_text: "Hello".to_string(),
    }));
    assert!(matches!(events.last(), Some(AgentEvent::RunFinished)));

    let snapshot = agent.watch_snapshot();
    assert_eq!(snapshot.borrow().status, AgentStatus::Finished);
    assert_eq!(snapshot.borrow().history_len, 2);
    assert!(snapshot.borrow().current_text.is_empty());
    assert!(snapshot.borrow().pending_tool_uses.is_empty());
}

#[tokio::test]
async fn send_failure_rolls_history_back_and_emits_run_failed() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![
            ok_stream(vec![
                ProviderEvent::MessageStarted {
                    id: "msg-1".to_string(),
                    model: model.id.clone(),
                    role: Role::Assistant,
                },
                ProviderEvent::ContentBlockStarted {
                    index: 0,
                    kind: ContentBlockStart::Text,
                },
                ProviderEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::Text("ok".to_string()),
                },
                ProviderEvent::ContentBlockStopped { index: 0 },
                ProviderEvent::MessageStopped,
            ]),
            erroring_stream(
                vec![ProviderEvent::MessageStarted {
                    id: "msg-2".to_string(),
                    model: model.id.clone(),
                    role: Role::Assistant,
                }],
                ProviderError::MalformedStream("boom".to_string()),
            ),
        ],
    );

    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();
    agent
        .send(vec![ContentBlock::Text {
            text: "first".to_string(),
        }])
        .await
        .unwrap();
    let baseline = agent.history().to_vec();
    let mut events = agent.subscribe_events();

    let result = agent
        .send(vec![ContentBlock::Text {
            text: "second".to_string(),
        }])
        .await;
    assert!(result.is_err());
    assert_eq!(agent.history(), baseline.as_slice());

    let events = collect_events(&mut events);
    assert!(matches!(events.last(), Some(AgentEvent::RunFailed { .. })));
}

fn collect_events(receiver: &mut tokio::sync::broadcast::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        events.push(event);
    }
    events
}
