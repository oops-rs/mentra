use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::time::{Duration, sleep};

use crate::{
    provider::model::{
        ContentBlock, ContentBlockDelta, ContentBlockStart, Message, ModelProviderKind,
        ProviderError, ProviderEvent, Request, Role, ToolChoice,
    },
    runtime::{Agent, AgentConfig, AgentEvent, BackgroundTaskStatus, Runtime, SpawnedAgentStatus},
};

use super::support::{
    ScriptedProvider, StaticTool, StreamScript, erroring_stream, model_info, ok_stream,
};

#[tokio::test]
async fn send_tool_use_turn_executes_tool_and_commits_follow_up_response() {
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
                    kind: ContentBlockStart::ToolUse {
                        id: "tool-1".to_string(),
                        name: "echo_tool".to_string(),
                    },
                },
                ProviderEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::ToolUseInputJson(r#"{"value":"hi"}"#.to_string()),
                },
                ProviderEvent::ContentBlockStopped { index: 0 },
                ProviderEvent::MessageStopped,
            ]),
            ok_stream(vec![
                ProviderEvent::MessageStarted {
                    id: "msg-2".to_string(),
                    model: model.id.clone(),
                    role: Role::Assistant,
                },
                ProviderEvent::ContentBlockStarted {
                    index: 0,
                    kind: ContentBlockStart::Text,
                },
                ProviderEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::Text("done".to_string()),
                },
                ProviderEvent::ContentBlockStopped { index: 0 },
                ProviderEvent::MessageStopped,
            ]),
        ],
    );

    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("echo_tool", "tool output"))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();
    let mut events = agent.subscribe_events();

    agent
        .send(vec![ContentBlock::Text {
            text: "hi".to_string(),
        }])
        .await
        .unwrap();

    assert_eq!(agent.history().len(), 4);
    assert_eq!(
        agent.history()[2],
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tool-1".to_string(),
                content: "tool output".to_string(),
                is_error: false,
            }],
        }
    );
    assert_eq!(
        agent.last_message(),
        Some(&Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "done".to_string(),
            }],
        })
    );

    let events = collect_events(&mut events);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolUseReady { .. }))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolExecutionFinished {
            result: ContentBlock::ToolResult {
                is_error: false,
                ..
            }
        }
    )));
}

#[tokio::test]
async fn tool_execution_error_is_wrapped_and_loop_continues() {
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
                    kind: ContentBlockStart::ToolUse {
                        id: "tool-1".to_string(),
                        name: "failing_tool".to_string(),
                    },
                },
                ProviderEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::ToolUseInputJson(r#"{"value":"hi"}"#.to_string()),
                },
                ProviderEvent::ContentBlockStopped { index: 0 },
                ProviderEvent::MessageStopped,
            ]),
            ok_stream(vec![
                ProviderEvent::MessageStarted {
                    id: "msg-2".to_string(),
                    model: model.id.clone(),
                    role: Role::Assistant,
                },
                ProviderEvent::ContentBlockStarted {
                    index: 0,
                    kind: ContentBlockStart::Text,
                },
                ProviderEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::Text("handled".to_string()),
                },
                ProviderEvent::ContentBlockStopped { index: 0 },
                ProviderEvent::MessageStopped,
            ]),
        ],
    );

    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::failure("failing_tool", "tool failed"))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "hi".to_string(),
        }])
        .await
        .unwrap();

    assert_eq!(
        agent.history()[2],
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tool-1".to_string(),
                content: "tool failed".to_string(),
                is_error: true,
            }],
        }
    );
    assert_eq!(
        agent.last_message(),
        Some(&Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "handled".to_string(),
            }],
        })
    );
}

#[tokio::test]
async fn background_run_tool_starts_task_and_continues_the_turn() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "tool-bg",
                "background_run",
                r#"{"command":"sleep 0.2; printf bg-done"}"#,
            ),
            text_stream(&model.id, "continued"),
        ],
    );

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();
    let mut events = agent.subscribe_events();

    agent
        .send(vec![ContentBlock::Text {
            text: "run background command".to_string(),
        }])
        .await
        .unwrap();

    assert_eq!(
        agent.history()[2],
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tool-bg".to_string(),
                content: "Started background task bg-1 for `sleep 0.2; printf bg-done`".to_string(),
                is_error: false,
            }],
        }
    );
    assert_eq!(
        agent.last_message(),
        Some(&Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "continued".to_string(),
            }],
        })
    );

    let background_tasks = agent.watch_snapshot().borrow().background_tasks.clone();
    assert_eq!(background_tasks.len(), 1);
    assert_eq!(background_tasks[0].status, BackgroundTaskStatus::Running);

    let events = collect_events(&mut events);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::BackgroundTaskStarted { task }
            if task.id == "bg-1" && task.command == "sleep 0.2; printf bg-done"
    )));
}

#[tokio::test]
async fn completed_background_results_are_injected_on_next_send() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "tool-bg",
                "background_run",
                r#"{"command":"sleep 0.05; printf bg-done"}"#,
            ),
            text_stream(&model.id, "continued"),
            text_stream(&model.id, "next turn"),
        ],
    );
    let provider_handle = provider.clone();

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "run background command".to_string(),
        }])
        .await
        .unwrap();
    wait_for_background_tasks(&agent, 1, BackgroundTaskStatus::Finished).await;

    agent
        .send(vec![ContentBlock::Text {
            text: "what finished?".to_string(),
        }])
        .await
        .unwrap();

    let requests = provider_handle.recorded_requests().await;
    let injected = latest_background_results_text(&requests[2]).expect("background results");
    assert!(injected.contains("<background-results>"));
    assert!(injected.contains("[bg:bg-1] status=finished"));
    assert!(injected.contains("command=\"sleep 0.05; printf bg-done\""));
    assert!(injected.contains("output=\"bg-done\""));
}

#[tokio::test]
async fn check_background_reports_single_task_and_lists_all_tasks() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "tool-bg",
                "background_run",
                r#"{"command":"sleep 0.05; printf bg-done"}"#,
            ),
            text_stream(&model.id, "started"),
            multi_tool_use_stream(
                &model.id,
                &[
                    ("check-one", "check_background", r#"{"task_id":"bg-1"}"#),
                    ("check-all", "check_background", r#"{}"#),
                ],
            ),
            text_stream(&model.id, "checked"),
        ],
    );

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "run background command".to_string(),
        }])
        .await
        .unwrap();
    wait_for_background_tasks(&agent, 1, BackgroundTaskStatus::Finished).await;

    agent
        .send(vec![ContentBlock::Text {
            text: "check it".to_string(),
        }])
        .await
        .unwrap();

    assert_eq!(
        agent.history()[7],
        Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "check-one".to_string(),
                    content: "[finished] sleep 0.05; printf bg-done\nbg-done".to_string(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "check-all".to_string(),
                    content: "bg-1: [finished] sleep 0.05; printf bg-done".to_string(),
                    is_error: false,
                },
            ],
        }
    );
}

#[tokio::test]
async fn completed_background_results_are_batched_in_completion_order() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![
            multi_tool_use_stream(
                &model.id,
                &[
                    (
                        "tool-bg-1",
                        "background_run",
                        r#"{"command":"sleep 0.02; printf first"}"#,
                    ),
                    (
                        "tool-bg-2",
                        "background_run",
                        r#"{"command":"sleep 0.05; printf second"}"#,
                    ),
                ],
            ),
            text_stream(&model.id, "continued"),
            text_stream(&model.id, "next turn"),
        ],
    );
    let provider_handle = provider.clone();

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "run two background commands".to_string(),
        }])
        .await
        .unwrap();
    wait_for_background_task_count(&agent, 2).await;
    wait_for_background_tasks(&agent, 2, BackgroundTaskStatus::Finished).await;

    agent
        .send(vec![ContentBlock::Text {
            text: "report completions".to_string(),
        }])
        .await
        .unwrap();

    let requests = provider_handle.recorded_requests().await;
    let injected = latest_background_results_text(&requests[2]).expect("background results");
    let first = injected.find("[bg:bg-1]").expect("first task line");
    let second = injected.find("[bg:bg-2]").expect("second task line");
    assert!(first < second);
    assert!(injected.contains("output=\"first\""));
    assert!(injected.contains("output=\"second\""));
}

#[tokio::test]
async fn failed_background_results_surface_in_snapshot_events_and_notifications() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "tool-bg",
                "background_run",
                r#"{"command":"sleep 0.05; echo boom >&2; exit 7"}"#,
            ),
            text_stream(&model.id, "continued"),
            text_stream(&model.id, "next turn"),
        ],
    );
    let provider_handle = provider.clone();

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();
    let mut events = agent.subscribe_events();

    agent
        .send(vec![ContentBlock::Text {
            text: "run failing background command".to_string(),
        }])
        .await
        .unwrap();
    wait_for_background_tasks(&agent, 1, BackgroundTaskStatus::Failed).await;

    let background_tasks = agent.watch_snapshot().borrow().background_tasks.clone();
    assert_eq!(background_tasks.len(), 1);
    assert_eq!(background_tasks[0].status, BackgroundTaskStatus::Failed);
    assert!(
        background_tasks[0]
            .output_preview
            .as_deref()
            .is_some_and(|preview| preview.contains("boom"))
    );

    agent
        .send(vec![ContentBlock::Text {
            text: "report failure".to_string(),
        }])
        .await
        .unwrap();

    let events = collect_events(&mut events);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::BackgroundTaskFinished { task }
            if task.id == "bg-1"
                && task.status == BackgroundTaskStatus::Failed
                && task.output_preview.as_deref().is_some_and(|preview| preview.contains("boom"))
    )));

    let requests = provider_handle.recorded_requests().await;
    let injected = latest_background_results_text(&requests[2]).expect("background results");
    assert!(injected.contains("status=failed"));
    assert!(injected.contains("output=\"boom\""));
}

#[tokio::test]
async fn drained_background_notifications_are_requeued_after_failed_run() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "tool-bg",
                "background_run",
                r#"{"command":"sleep 0.05; printf bg-done"}"#,
            ),
            text_stream(&model.id, "continued"),
            erroring_stream(
                vec![ProviderEvent::MessageStarted {
                    id: "msg-fail".to_string(),
                    model: model.id.clone(),
                    role: Role::Assistant,
                }],
                ProviderError::MalformedStream("boom".to_string()),
            ),
            text_stream(&model.id, "retried"),
        ],
    );
    let provider_handle = provider.clone();

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "run background command".to_string(),
        }])
        .await
        .unwrap();
    wait_for_background_tasks(&agent, 1, BackgroundTaskStatus::Finished).await;

    let failed = agent
        .send(vec![ContentBlock::Text {
            text: "this turn fails".to_string(),
        }])
        .await;
    assert!(failed.is_err());

    agent
        .send(vec![ContentBlock::Text {
            text: "retry".to_string(),
        }])
        .await
        .unwrap();

    let requests = provider_handle.recorded_requests().await;
    let failed_request_results =
        latest_background_results_text(&requests[2]).expect("background results on failed run");
    let retried_request_results =
        latest_background_results_text(&requests[3]).expect("background results on retried run");
    assert_eq!(failed_request_results, retried_request_results);
}

#[tokio::test]
async fn default_runtime_exposes_task_and_new_empty_does_not() {
    let model = model_info("model", ModelProviderKind::Anthropic);

    let default_provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![text_stream(&model.id, "ok")],
    );
    let default_handle = default_provider.clone();
    let default_runtime = Runtime::builder()
        .with_provider_instance(default_provider)
        .build()
        .expect("build runtime");
    let mut default_agent = default_runtime.spawn("agent", model.clone()).unwrap();
    default_agent
        .send(vec![ContentBlock::Text {
            text: "hello".to_string(),
        }])
        .await
        .unwrap();

    let default_requests = default_handle.recorded_requests().await;
    let default_tools = tool_names(&default_requests[0]);
    assert!(default_tools.contains("bash"));
    assert!(default_tools.contains("background_run"));
    assert!(default_tools.contains("check_background"));
    assert!(default_tools.contains("compact"));
    assert!(default_tools.contains("read_file"));
    assert!(default_tools.contains("task"));
    assert!(default_tools.contains("task_create"));
    assert!(default_tools.contains("task_update"));
    assert!(default_tools.contains("task_list"));
    assert!(default_tools.contains("task_get"));
    assert!(!default_tools.contains("load_skill"));

    let empty_provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![text_stream(&model.id, "ok")],
    );
    let empty_handle = empty_provider.clone();
    let empty_runtime = Runtime::empty_builder()
        .with_provider_instance(empty_provider)
        .build()
        .expect("build runtime");
    let mut empty_agent = empty_runtime.spawn("agent", model).unwrap();
    empty_agent
        .send(vec![ContentBlock::Text {
            text: "hello".to_string(),
        }])
        .await
        .unwrap();

    let empty_requests = empty_handle.recorded_requests().await;
    let empty_tools = tool_names(&empty_requests[0]);
    assert!(!empty_tools.contains("background_run"));
    assert!(!empty_tools.contains("check_background"));
    assert!(!empty_tools.contains("compact"));
    assert!(!empty_tools.contains("task"));
    assert!(!empty_tools.contains("task_create"));
    assert!(!empty_tools.contains("task_update"));
    assert!(!empty_tools.contains("task_list"));
    assert!(!empty_tools.contains("task_get"));
    assert!(!empty_tools.contains("load_skill"));
}

#[tokio::test]
async fn registered_skills_are_exposed_and_load_skill_returns_wrapped_content() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "tool-skill", "load_skill", r#"{"name":"git"}"#),
            text_stream(&model.id, "done"),
        ],
    );
    let provider_handle = provider.clone();

    let skills_dir = temp_skills_dir("load-skill");
    write_skill(
        &skills_dir,
        "git",
        "---\nname: git\ndescription: Git workflow helpers\n---\nUse feature branches.\nRun tests first.\n",
    );
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_skills_dir(&skills_dir)
        .expect("register skills")
        .build()
        .expect("build runtime");
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
            text: "hello".to_string(),
        }])
        .await
        .unwrap();

    assert_eq!(
        agent.history()[2],
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tool-skill".to_string(),
                content: "<skill name=\"git\">\nUse feature branches.\nRun tests first.\n</skill>"
                    .to_string(),
                is_error: false,
            }],
        }
    );

    let requests = provider_handle.recorded_requests().await;
    let tools = tool_names(&requests[0]);
    assert!(tools.contains("load_skill"));
    assert_eq!(
        requests[0].system.as_deref(),
        Some(
            "Base system prompt\n\nSkills available:\n  - git: Git workflow helpers\nUse the load_skill tool only when one of these skills is relevant to the task."
        )
    );
}

#[tokio::test]
async fn task_subagent_keeps_load_skill_while_hiding_task() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "tool-parent",
                "task",
                r#"{"prompt":"inspect repo"}"#,
            ),
            text_stream(&model.id, "child summary"),
            text_stream(&model.id, "parent done"),
        ],
    );
    let provider_handle = provider.clone();

    let skills_dir = temp_skills_dir("subagent-skills");
    write_skill(
        &skills_dir,
        "review",
        "---\nname: review\ndescription: Code review checklist\n---\nCheck tests.\n",
    );
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .with_skills_dir(&skills_dir)
        .expect("register skills")
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "delegate".to_string(),
        }])
        .await
        .unwrap();

    let requests = provider_handle.recorded_requests().await;
    let child_tools = tool_names(&requests[1]);
    assert!(child_tools.contains("load_skill"));
    assert!(!child_tools.contains("task"));
    assert_eq!(
        requests[1].system.as_deref(),
        Some(
            "You are a subagent working for another agent. Solve the delegated task, use tools when helpful, and finish with a concise final answer for the parent agent.\n\nSkills available:\n  - review: Code review checklist\nUse the load_skill tool only when one of these skills is relevant to the task."
        )
    );
}

#[tokio::test]
async fn task_tool_runs_child_with_isolated_history_and_filtered_tools() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "tool-parent",
                "task",
                r#"{"prompt":"inspect repo"}"#,
            ),
            text_stream(&model.id, "child summary"),
            text_stream(&model.id, "parent done"),
        ],
    );
    let provider_handle = provider.clone();

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model.clone()).unwrap();
    let mut events = agent.subscribe_events();

    agent
        .send(vec![ContentBlock::Text {
            text: "delegate".to_string(),
        }])
        .await
        .unwrap();

    assert_eq!(agent.history().len(), 4);
    assert_eq!(
        agent.history()[2],
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tool-parent".to_string(),
                content: "child summary".to_string(),
                is_error: false,
            }],
        }
    );
    assert_eq!(
        agent.last_message(),
        Some(&Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "parent done".to_string(),
            }],
        })
    );

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].messages.len(), 1);
    assert_eq!(requests[1].messages[0].role, Role::User);
    assert_eq!(
        requests[1].messages[0].content,
        vec![ContentBlock::Text {
            text: "inspect repo".to_string(),
        }]
    );

    let child_tools = tool_names(&requests[1]);
    assert!(child_tools.contains("bash"));
    assert!(child_tools.contains("read_file"));
    assert!(!child_tools.contains("task"));

    let subagents = agent.watch_snapshot().borrow().subagents.clone();
    assert_eq!(subagents.len(), 1);
    assert_eq!(subagents[0].name, "agent::task");
    assert_eq!(subagents[0].model, model.id);
    assert_eq!(subagents[0].status, SpawnedAgentStatus::Finished);

    let events = collect_events(&mut events);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::SubagentSpawned { .. }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::SubagentFinished { .. }))
    );
}

#[tokio::test]
async fn task_subagent_does_not_force_hidden_task_tool_choice() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "tool-parent",
                "task",
                r#"{"prompt":"inspect repo"}"#,
            ),
            text_stream(&model.id, "child summary"),
            text_stream(&model.id, "parent done"),
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
                tool_choice: Some(ToolChoice::Tool {
                    name: "task".to_string(),
                }),
                ..AgentConfig::default()
            },
        )
        .unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "delegate".to_string(),
        }])
        .await
        .unwrap();

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(
        requests[0].tool_choice,
        Some(ToolChoice::Tool {
            name: "task".to_string(),
        })
    );
    assert_eq!(requests[1].tool_choice, Some(ToolChoice::Auto));
    assert!(!tool_names(&requests[1]).contains("task"));
}

#[tokio::test]
async fn task_tool_wraps_child_failure_and_parent_continues() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "tool-parent",
                "task",
                r#"{"prompt":"inspect repo"}"#,
            ),
            erroring_stream(
                vec![ProviderEvent::MessageStarted {
                    id: "child-msg".to_string(),
                    model: model.id.clone(),
                    role: Role::Assistant,
                }],
                ProviderError::MalformedStream("boom".to_string()),
            ),
            text_stream(&model.id, "handled"),
        ],
    );

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "delegate".to_string(),
        }])
        .await
        .unwrap();

    assert_eq!(
        agent.history()[2],
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tool-parent".to_string(),
                content: "Subagent failed: FailedToStreamResponse(MalformedStream(\"boom\"))"
                    .to_string(),
                is_error: true,
            }],
        }
    );
    assert_eq!(
        agent.last_message(),
        Some(&Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "handled".to_string(),
            }],
        })
    );

    let subagents = agent.watch_snapshot().borrow().subagents.clone();
    assert_eq!(subagents.len(), 1);
    assert!(matches!(
        &subagents[0].status,
        SpawnedAgentStatus::Failed(message)
            if message == "FailedToStreamResponse(MalformedStream(\"boom\"))"
    ));
}

#[tokio::test]
async fn child_rejects_nested_task_requests_without_recursing() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "parent-task", "task", r#"{"prompt":"delegate"}"#),
            tool_use_stream(&model.id, "child-task", "task", r#"{"prompt":"recurse"}"#),
            text_stream(&model.id, "child recovered"),
            text_stream(&model.id, "parent done"),
        ],
    );
    let provider_handle = provider.clone();

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "delegate".to_string(),
        }])
        .await
        .unwrap();

    assert_eq!(
        agent.history()[2],
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "parent-task".to_string(),
                content: "child recovered".to_string(),
                is_error: false,
            }],
        }
    );

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 4);
    assert!(!tool_names(&requests[1]).contains("task"));
    assert_eq!(requests[2].messages.len(), 3);
    assert_eq!(
        requests[2].messages[2],
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "child-task".to_string(),
                content: "Tool 'task' is not available for this agent".to_string(),
                is_error: true,
            }],
        }
    );
}

#[tokio::test]
async fn task_tool_returns_error_when_child_hits_round_limit() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let mut scripts = vec![tool_use_stream(
        &model.id,
        "parent-task",
        "task",
        r#"{"prompt":"delegate"}"#,
    )];
    for index in 0..30 {
        scripts.push(tool_use_stream(
            &model.id,
            &format!("child-tool-{index}"),
            "echo_tool",
            r#"{"value":"ping"}"#,
        ));
    }
    scripts.push(text_stream(&model.id, "parent handled"));

    let provider =
        ScriptedProvider::new(ModelProviderKind::Anthropic, vec![model.clone()], scripts);
    let provider_handle = provider.clone();

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("echo_tool", "pong"))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "delegate".to_string(),
        }])
        .await
        .unwrap();

    assert_eq!(
        agent.history()[2],
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "parent-task".to_string(),
                content: "Subagent failed: MaxRoundsExceeded(30)".to_string(),
                is_error: true,
            }],
        }
    );
    assert_eq!(
        agent.last_message(),
        Some(&Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "parent handled".to_string(),
            }],
        })
    );

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 32);
}

fn collect_events(receiver: &mut tokio::sync::broadcast::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        events.push(event);
    }
    events
}

async fn wait_for_background_task_count(agent: &Agent, expected_count: usize) {
    for _ in 0..200 {
        if agent.watch_snapshot().borrow().background_tasks.len() == expected_count {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }

    panic!("timed out waiting for {expected_count} background tasks");
}

async fn wait_for_background_tasks(
    agent: &Agent,
    expected_count: usize,
    status: BackgroundTaskStatus,
) {
    for _ in 0..200 {
        let background_tasks = agent.watch_snapshot().borrow().background_tasks.clone();
        if background_tasks.len() == expected_count
            && background_tasks.iter().all(|task| task.status == status)
        {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }

    panic!(
        "timed out waiting for {expected_count} background tasks to reach {:?}",
        status
    );
}

fn latest_background_results_text<'a>(request: &'a Request<'a>) -> Option<&'a str> {
    request
        .messages
        .iter()
        .rev()
        .flat_map(|message| message.content.iter())
        .find_map(|block| match block {
            ContentBlock::Text { text } if text.contains("<background-results>") => {
                Some(text.as_str())
            }
            _ => None,
        })
}

fn text_stream(model: &str, text: &str) -> StreamScript {
    ok_stream(vec![
        ProviderEvent::MessageStarted {
            id: format!("msg-{text}"),
            model: model.to_string(),
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

fn tool_use_stream(model: &str, id: &str, name: &str, input_json: &str) -> StreamScript {
    ok_stream(vec![
        ProviderEvent::MessageStarted {
            id: format!("msg-{id}"),
            model: model.to_string(),
            role: Role::Assistant,
        },
        ProviderEvent::ContentBlockStarted {
            index: 0,
            kind: ContentBlockStart::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
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

fn multi_tool_use_stream(model: &str, calls: &[(&str, &str, &str)]) -> StreamScript {
    let mut events = vec![ProviderEvent::MessageStarted {
        id: "msg-multi-tool".to_string(),
        model: model.to_string(),
        role: Role::Assistant,
    }];

    for (index, (id, name, input_json)) in calls.iter().enumerate() {
        events.push(ProviderEvent::ContentBlockStarted {
            index,
            kind: ContentBlockStart::ToolUse {
                id: (*id).to_string(),
                name: (*name).to_string(),
            },
        });
        events.push(ProviderEvent::ContentBlockDelta {
            index,
            delta: ContentBlockDelta::ToolUseInputJson((*input_json).to_string()),
        });
        events.push(ProviderEvent::ContentBlockStopped { index });
    }

    events.push(ProviderEvent::MessageStopped);
    ok_stream(events)
}

fn tool_names(request: &Request<'_>) -> std::collections::HashSet<String> {
    request.tools.iter().map(|tool| tool.name.clone()).collect()
}

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

fn temp_skills_dir(label: &str) -> PathBuf {
    let unique = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "mentra-runtime-skills-{label}-{timestamp}-{unique}"
    ));
    fs::create_dir_all(&path).expect("create temp dir");
    path
}

fn write_skill(root: &Path, name: &str, content: &str) {
    let skill_dir = root.join(name);
    fs::create_dir_all(&skill_dir).expect("create skill dir");
    fs::write(skill_dir.join("SKILL.md"), content).expect("write skill");
}
