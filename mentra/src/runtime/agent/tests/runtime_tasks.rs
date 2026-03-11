use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    ContentBlock, Message, ModelProviderKind, Role,
    provider::{ContentBlockDelta, ContentBlockStart, ProviderError, ProviderEvent},
    runtime::{
        AgentConfig, ContextCompactionConfig, Runtime, TaskGraphConfig, TaskItem, TaskStatus,
        task_graph::TASK_REMINDER_TEXT,
    },
};

use super::support::{ScriptedProvider, erroring_stream, model_info, ok_stream};

#[tokio::test]
async fn task_graph_updates_snapshot_and_persists_for_new_agents() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let tasks_dir = temp_tasks_dir("persist");
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![
            task_tool_stream(
                "tool-1",
                "task_create",
                r#"{"subject":"Plan work","owner":"agent-a"}"#,
            ),
            text_stream("created"),
        ],
    );

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let config = task_graph_config(tasks_dir.clone());
    let mut agent = runtime
        .spawn_with_config("agent", model.clone(), config.clone())
        .expect("spawn agent");

    agent
        .send(vec![ContentBlock::Text {
            text: "start".to_string(),
        }])
        .await
        .expect("send");

    assert_eq!(
        agent.watch_snapshot().borrow().tasks,
        vec![TaskItem {
            id: 1,
            subject: "Plan work".to_string(),
            description: String::new(),
            status: TaskStatus::Pending,
            blocked_by: Vec::new(),
            blocks: Vec::new(),
            owner: "agent-a".to_string(),
        }]
    );
    assert!(tasks_dir.join("task_1.json").exists());

    let other_provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![text_stream("ok")],
    );
    let other_runtime = Runtime::builder()
        .with_provider_instance(other_provider)
        .build()
        .expect("build runtime");
    let other_agent = other_runtime
        .spawn_with_config("other", model, config)
        .expect("spawn other agent");
    assert_eq!(other_agent.watch_snapshot().borrow().tasks.len(), 1);
}

#[tokio::test]
async fn task_reminder_is_injected_after_three_rounds_without_task_tools() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let tasks_dir = temp_tasks_dir("reminder");
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![
            task_tool_stream("tool-1", "task_create", r#"{"subject":"Plan work"}"#),
            text_stream("created"),
            text_stream("round 1"),
            text_stream("round 2"),
            text_stream("round 3"),
            text_stream("round 4"),
        ],
    );
    let provider_handle = provider.clone();

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                system: Some("Base system prompt".to_string()),
                task_graph: TaskGraphConfig {
                    tasks_dir,
                    reminder_threshold: 3,
                },
                ..AgentConfig::default()
            },
        )
        .expect("spawn agent");

    agent
        .send(vec![ContentBlock::Text {
            text: "set task".to_string(),
        }])
        .await
        .expect("create task");

    for round in 1..=4 {
        agent
            .send(vec![ContentBlock::Text {
                text: format!("round {round}"),
            }])
            .await
            .expect("send round");
    }

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 6);
    assert_eq!(requests[0].system.as_deref(), Some("Base system prompt"));
    assert_eq!(requests[3].system.as_deref(), Some("Base system prompt"));

    let expected_system = format!("{TASK_REMINDER_TEXT}\n\nBase system prompt");
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
async fn task_graph_state_rolls_back_when_run_fails() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let tasks_dir = temp_tasks_dir("rollback");
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![
            task_tool_stream("tool-1", "task_create", r#"{"subject":"Plan work"}"#),
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

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config("agent", model, task_graph_config(tasks_dir.clone()))
        .expect("spawn agent");

    let result = agent
        .send(vec![ContentBlock::Text {
            text: "create task".to_string(),
        }])
        .await;

    assert!(result.is_err());
    assert!(agent.history().is_empty());
    assert!(agent.watch_snapshot().borrow().tasks.is_empty());
    assert!(!tasks_dir.join("task_1.json").exists());
}

#[tokio::test]
async fn task_graph_survives_auto_compaction() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let tasks_dir = temp_tasks_dir("compact");
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![
            task_tool_stream("tool-1", "task_create", r#"{"subject":"Plan work"}"#),
            text_stream("created"),
            text_stream("summary"),
            text_stream("after compact"),
        ],
    );

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                task_graph: TaskGraphConfig {
                    tasks_dir,
                    reminder_threshold: 3,
                },
                context_compaction: ContextCompactionConfig {
                    auto_compact_threshold_tokens: Some(500),
                    ..ContextCompactionConfig::default()
                },
                ..AgentConfig::default()
            },
        )
        .expect("spawn agent");

    agent
        .send(vec![ContentBlock::Text {
            text: "create task".to_string(),
        }])
        .await
        .expect("create task");
    agent
        .send(vec![ContentBlock::Text {
            text: "trigger compact ".repeat(100),
        }])
        .await
        .expect("trigger compact");

    assert_eq!(agent.watch_snapshot().borrow().tasks.len(), 1);
    assert!(matches!(
        &agent.history()[0],
        Message {
            role: Role::User,
            content,
        } if matches!(content.first(), Some(ContentBlock::Text { text }) if text.starts_with("[Compressed context]"))
    ));
}

fn task_graph_config(tasks_dir: PathBuf) -> AgentConfig {
    AgentConfig {
        task_graph: TaskGraphConfig {
            tasks_dir,
            reminder_threshold: 3,
        },
        ..AgentConfig::default()
    }
}

fn task_tool_stream(
    tool_id: &str,
    tool_name: &str,
    input_json: &str,
) -> super::support::StreamScript {
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
                name: tool_name.to_string(),
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

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

fn temp_tasks_dir(label: &str) -> PathBuf {
    let unique = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let path =
        std::env::temp_dir().join(format!("mentra-task-runtime-{label}-{timestamp}-{unique}"));
    fs::create_dir_all(&path).expect("create temp dir");
    path
}
