use crate::{
    provider::model::{
        ContentBlock, ContentBlockDelta, ContentBlockStart, Message, ModelProviderKind,
        ProviderError, ProviderEvent, Role,
    },
    runtime::todo::TODO_REMINDER_TEXT,
    runtime::{AgentConfig, Runtime, TodoItem, TodoStatus},
};

use super::support::{ScriptedProvider, erroring_stream, model_info, ok_stream};

#[tokio::test]
async fn todo_updates_snapshot_and_returns_rendered_checklist() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![
            todo_stream(
                "tool-1",
                r#"{"items":[{"id":"task-a","text":"Plan work","status":"in_progress"},{"id":"task-b","text":"Ship tests","status":"pending"}]}"#,
            ),
            text_stream("done"),
        ],
    );

    let mut runtime = Runtime::new_empty();
    runtime.register_provider_instance(provider);
    let mut agent = runtime.spawn("agent", model).unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "start".to_string(),
        }])
        .await
        .unwrap();

    assert_eq!(
        agent.history()[2],
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tool-1".to_string(),
                content: "[>] task-a: Plan work\n[ ] task-b: Ship tests".to_string(),
                is_error: false,
            }],
        }
    );
    assert_eq!(
        agent.watch_snapshot().borrow().todos,
        vec![
            TodoItem {
                id: "task-a".to_string(),
                text: "Plan work".to_string(),
                status: TodoStatus::InProgress,
            },
            TodoItem {
                id: "task-b".to_string(),
                text: "Ship tests".to_string(),
                status: TodoStatus::Pending,
            },
        ]
    );
}

#[tokio::test]
async fn todo_rejects_multiple_in_progress_items() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![
            todo_stream(
                "tool-1",
                r#"{"items":[{"id":"task-a","text":"Plan work","status":"in_progress"},{"id":"task-b","text":"Ship tests","status":"in_progress"}]}"#,
            ),
            text_stream("handled"),
        ],
    );

    let mut runtime = Runtime::new_empty();
    runtime.register_provider_instance(provider);
    let mut agent = runtime.spawn("agent", model).unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "start".to_string(),
        }])
        .await
        .unwrap();

    assert_eq!(
        agent.history()[2],
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tool-1".to_string(),
                content: "Only one todo item can be in_progress".to_string(),
                is_error: true,
            }],
        }
    );
    assert!(agent.watch_snapshot().borrow().todos.is_empty());
}

#[tokio::test]
async fn todo_rejects_duplicate_ids() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![
            todo_stream(
                "tool-1",
                r#"{"items":[{"id":"task-a","text":"Plan work","status":"pending"},{"id":"task-a","text":"Ship tests","status":"completed"}]}"#,
            ),
            text_stream("handled"),
        ],
    );

    let mut runtime = Runtime::new_empty();
    runtime.register_provider_instance(provider);
    let mut agent = runtime.spawn("agent", model).unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "start".to_string(),
        }])
        .await
        .unwrap();

    assert_eq!(
        agent.history()[2],
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tool-1".to_string(),
                content: "Duplicate todo item id 'task-a'".to_string(),
                is_error: true,
            }],
        }
    );
}

#[tokio::test]
async fn todo_rejects_missing_required_fields() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![
            todo_stream(
                "tool-1",
                r#"{"items":[{"id":"task-a","status":"pending"}]}"#,
            ),
            text_stream("handled"),
        ],
    );

    let mut runtime = Runtime::new_empty();
    runtime.register_provider_instance(provider);
    let mut agent = runtime.spawn("agent", model).unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "start".to_string(),
        }])
        .await
        .unwrap();

    let Message { content, .. } = &agent.history()[2];
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = &content[0]
    else {
        panic!("expected tool result");
    };

    assert!(*is_error);
    assert!(content.starts_with("Invalid todo input:"));
}

#[tokio::test]
async fn reminder_is_injected_after_three_rounds_without_todo() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![
            todo_stream(
                "tool-1",
                r#"{"items":[{"id":"task-a","text":"Plan work","status":"pending"}]}"#,
            ),
            text_stream("todo saved"),
            text_stream("round 1"),
            text_stream("round 2"),
            text_stream("round 3"),
            text_stream("round 4"),
        ],
    );
    let provider_handle = provider.clone();

    let mut runtime = Runtime::new_empty();
    runtime.register_provider_instance(provider);
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                system: Some("Base system prompt".to_string()),
                ..AgentConfig::default()
            },
        )
        .unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "set todos".to_string(),
        }])
        .await
        .unwrap();

    for round in 1..=4 {
        agent
            .send(vec![ContentBlock::Text {
                text: format!("round {round}"),
            }])
            .await
            .unwrap();
    }

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 6);
    assert_eq!(requests[0].system.as_deref(), Some("Base system prompt"));
    assert_eq!(requests[3].system.as_deref(), Some("Base system prompt"));
    let expected_system = format!("{TODO_REMINDER_TEXT}\n\nBase system prompt");
    assert_eq!(
        requests[4].system.as_deref(),
        Some(expected_system.as_str())
    );
    assert_eq!(
        requests[5].system.as_deref(),
        Some(expected_system.as_str())
    );
}

#[tokio::test]
async fn completed_todos_do_not_trigger_reminders() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![
            todo_stream(
                "tool-1",
                r#"{"items":[{"id":"task-a","text":"Done","status":"completed"}]}"#,
            ),
            text_stream("todo saved"),
            text_stream("round 1"),
            text_stream("round 2"),
            text_stream("round 3"),
            text_stream("round 4"),
        ],
    );
    let provider_handle = provider.clone();

    let mut runtime = Runtime::new_empty();
    runtime.register_provider_instance(provider);
    let mut agent = runtime.spawn("agent", model).unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "set todos".to_string(),
        }])
        .await
        .unwrap();

    for round in 1..=4 {
        agent
            .send(vec![ContentBlock::Text {
                text: format!("round {round}"),
            }])
            .await
            .unwrap();
    }

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 6);
    assert!(requests.iter().all(|request| request.system.is_none()));
}

#[tokio::test]
async fn todo_state_rolls_back_when_run_fails() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![
            todo_stream(
                "tool-1",
                r#"{"items":[{"id":"task-a","text":"Plan work","status":"pending"}]}"#,
            ),
            erroring_stream(
                vec![ProviderEvent::MessageStarted {
                    id: "msg-fail".to_string(),
                    model: model.id.clone(),
                    role: Role::Assistant,
                }],
                ProviderError::MalformedStream("boom".to_string()),
            ),
        ],
    );

    let mut runtime = Runtime::new_empty();
    runtime.register_provider_instance(provider);
    let mut agent = runtime.spawn("agent", model).unwrap();

    let result = agent
        .send(vec![ContentBlock::Text {
            text: "set todos".to_string(),
        }])
        .await;

    assert!(result.is_err());
    assert!(agent.history().is_empty());
    assert!(agent.watch_snapshot().borrow().todos.is_empty());
}

fn todo_stream(tool_id: &str, input_json: &str) -> super::support::StreamScript {
    ok_stream(vec![
        ProviderEvent::MessageStarted {
            id: format!("msg-{tool_id}"),
            model: "model".to_string(),
            role: Role::Assistant,
        },
        ProviderEvent::ContentBlockStarted {
            index: 0,
            kind: ContentBlockStart::ToolUse {
                id: tool_id.to_string(),
                name: "todo".to_string(),
            },
        },
        ProviderEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::ToolUseInputJson(input_json.to_string()),
        },
        ProviderEvent::ContentBlockStopped { index: 0 },
        ProviderEvent::MessageStopped,
    ])
}

fn text_stream(text: &str) -> super::support::StreamScript {
    ok_stream(vec![
        ProviderEvent::MessageStarted {
            id: format!("msg-{text}"),
            model: "model".to_string(),
            role: Role::Assistant,
        },
        ProviderEvent::ContentBlockStarted {
            index: 0,
            kind: ContentBlockStart::Text,
        },
        ProviderEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::Text(text.to_string()),
        },
        ProviderEvent::ContentBlockStopped { index: 0 },
        ProviderEvent::MessageStopped,
    ])
}
