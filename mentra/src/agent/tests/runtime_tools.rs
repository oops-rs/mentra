use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
#[cfg(unix)]
use tokio::time::timeout;
use tokio::time::{Duration, sleep};

use crate::{
    BackgroundTaskStatus, BuiltinProvider, ContentBlock, Message, Role,
    agent::{
        Agent, AgentConfig, AgentEvent, MemoryConfig, SpawnedAgentStatus, TaskConfig,
        TeamAutonomyConfig, TeamConfig, ToolProfile, WorkspaceConfig,
    },
    memory::{MemoryRecord, MemoryRecordKind, MemoryStore},
    provider::{
        ContentBlockDelta, ContentBlockStart, ProviderError, ProviderEvent, Request, ToolChoice,
        ToolSearchMode,
    },
    runtime::{
        CancellationToken, HybridRuntimeStore, RunOptions, Runtime, RuntimeError, RuntimePolicy,
        SqliteRuntimeStore, TaskIntrinsicTool,
        task::{self, TaskAccess},
    },
    team::{TeamMemberStatus, TeamMessageKind, TeamProtocolStatus},
};

use super::support::{
    ProbeTool, ScriptedProvider, StaticTool, StreamScript, background_failure_command,
    background_success_command, command_input_json, command_input_with_working_directory_json,
    controlled_stream, erroring_stream, model_info, ok_stream, shell_pwd_command,
};

#[tokio::test]
async fn send_tool_use_turn_executes_tool_and_commits_follow_up_response() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
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
        Message::user(ContentBlock::ToolResult {
            tool_use_id: "tool-1".to_string(),
            content: "tool output".into(),
            is_error: false,
        })
    );
    assert_eq!(
        agent.last_message(),
        Some(&Message::assistant(ContentBlock::text("done")))
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
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
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

    let provider_handle = provider.clone();
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
        Message::user(ContentBlock::ToolResult {
            tool_use_id: "tool-1".to_string(),
            content: "tool failed".into(),
            is_error: true,
        })
    );
    assert_eq!(
        agent.last_message(),
        Some(&Message::assistant(ContentBlock::text("handled")))
    );

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 2);
    assert!(matches!(
        &requests[1].messages[2].content[0],
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error: true,
        } if tool_use_id == "tool-1" && content.to_display_string() == "tool failed"
    ));
}

#[tokio::test]
async fn malformed_tool_json_is_reported_back_to_model_instead_of_aborting() {
    let model = model_info("model", BuiltinProvider::OpenAI);
    let provider = ScriptedProvider::new(
        BuiltinProvider::OpenAI,
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
                        name: "files".to_string(),
                    },
                },
                ProviderEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::ToolUseInputJson(
                        r#"{"path":"src/main.rs"#.to_string(),
                    ),
                },
                ProviderEvent::ContentBlockStopped { index: 0 },
                ProviderEvent::MessageStopped,
            ]),
            text_stream(&model.id, "recovered"),
        ],
    );

    let provider_handle = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();
    let mut events = agent.subscribe_events();

    agent
        .send(vec![ContentBlock::Text {
            text: "inspect the file".to_string(),
        }])
        .await
        .unwrap();

    assert_eq!(agent.history().len(), 3);
    assert_eq!(
        agent.history()[1],
        Message::user(ContentBlock::text(
            "One or more tool calls could not be executed because their JSON arguments were invalid. Please retry with valid JSON that matches the tool schema exactly.\n\nTool 'files' (tool-1) failed to parse: EOF while parsing a string at line 1 column 20.\nRaw arguments (truncated): {\"path\":\"src/main.rs"
        ))
    );
    assert_eq!(
        agent.last_message(),
        Some(&Message::assistant(ContentBlock::text("recovered")))
    );

    let events = collect_events(&mut events);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolUseReady { .. }))
    );

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].messages.len(), 2);
    assert_eq!(
        requests[1].messages[1],
        Message::user(ContentBlock::text(
            "One or more tool calls could not be executed because their JSON arguments were invalid. Please retry with valid JSON that matches the tool schema exactly.\n\nTool 'files' (tool-1) failed to parse: EOF while parsing a string at line 1 column 20.\nRaw arguments (truncated): {\"path\":\"src/main.rs"
        ))
    );
}

#[tokio::test]
async fn background_run_tool_starts_task_and_continues_the_turn() {
    let command = background_success_command("bg-done", 200);
    let input = command_input_json(&command);
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "tool-bg", "background_run", &input),
            text_stream(&model.id, "continued"),
        ],
    );

    let runtime = Runtime::builder()
        .with_policy(RuntimePolicy::permissive())
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
    let cwd = agent.config().workspace.base_dir.display().to_string();

    assert_eq!(
        agent.history()[2],
        Message::user(ContentBlock::ToolResult {
            tool_use_id: "tool-bg".to_string(),
            content: format!("Started background task bg-1 in {cwd} for `{command}`").into(),
            is_error: false,
        })
    );
    assert_eq!(
        agent.last_message(),
        Some(&Message::assistant(ContentBlock::text("continued")))
    );

    let background_tasks = agent.watch_snapshot().borrow().background_tasks.clone();
    assert_eq!(background_tasks.len(), 1);
    assert_eq!(background_tasks[0].status, BackgroundTaskStatus::Running);

    let events = collect_events(&mut events);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::BackgroundTaskStarted { task }
            if task.id == "bg-1" && task.command == command
    )));
}

#[tokio::test]
async fn completed_background_results_are_injected_on_next_send() {
    let command = background_success_command("bg-done", 50);
    let input = command_input_json(&command);
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "tool-bg", "background_run", &input),
            text_stream(&model.id, "continued"),
            text_stream(&model.id, "next turn"),
        ],
    );
    let provider_handle = provider.clone();

    let runtime = Runtime::builder()
        .with_store(temp_store("bg-results-next-send"))
        .with_policy(RuntimePolicy::permissive())
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
    assert!(injected.contains(&format!("command=\"{command}\"")));
    assert!(injected.contains("output=\"bg-done\""));
}

#[tokio::test]
async fn teammate_auto_wakes_after_background_task_finishes() {
    let command = background_success_command("bg-done", 50);
    let input = command_input_json(&command);
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "tool-bg", "background_run", &input),
            text_stream(&model.id, "started"),
            text_stream(&model.id, "processed background result"),
        ],
    );
    let provider_handle = provider.clone();

    let workspace_dir = temp_team_dir("teammate-background-autowake");
    let workspace_dir = fs::canonicalize(&workspace_dir).expect("canonicalize workspace dir");
    let team_dir = temp_team_dir("teammate-background-autowake-team");
    let store = temp_store("teammate-background-autowake");
    let runtime = Runtime::builder()
        .with_store(store.clone())
        .with_policy(RuntimePolicy::permissive().with_allowed_working_root(std::env::temp_dir()))
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model,
            AgentConfig {
                team: team_config(team_dir),
                workspace: workspace_config(&workspace_dir),
                ..Default::default()
            },
        )
        .expect("spawn lead");

    lead.spawn_teammate(
        "alice",
        "coder",
        Some("Start a background command, then keep going when it finishes.".to_string()),
    )
    .await
    .expect("spawn teammate");

    let teammate_id = lead.watch_snapshot().borrow().teammates[0].id.clone();
    wait_for_recorded_requests(&provider_handle, 2).await;
    wait_for_background_task_record(&store, &teammate_id, 1).await;
    wait_for_background_task_status(&store, &teammate_id, "bg-1", BackgroundTaskStatus::Finished)
        .await;
    wait_for_recorded_requests(&provider_handle, 3).await;
    wait_for_teammate_status(&lead, TeamMemberStatus::Idle).await;

    let requests = provider_handle.recorded_requests().await;
    let injected = latest_background_results_text(&requests[2]).expect("background results");
    assert!(injected.contains("[bg:bg-1] status=finished"));
    assert!(request_contains_text(
        &requests[2],
        "Review any completed background task results"
    ));
}

#[tokio::test]
async fn check_background_reports_single_task_and_lists_all_tasks() {
    let command = background_success_command("bg-done", 50);
    let input = command_input_json(&command);
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "tool-bg", "background_run", &input),
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
        .with_store(temp_store("bg-check-reports"))
        .with_policy(RuntimePolicy::permissive())
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
    let cwd = agent.config().workspace.base_dir.display().to_string();

    assert_eq!(
        agent.history()[7],
        Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "check-one".to_string(),
                    content: format!("[finished] cwd={cwd}\n{command}\nbg-done").into(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "check-all".to_string(),
                    content: format!("bg-1: [finished] cwd={cwd} {command}").into(),
                    is_error: false,
                },
            ],
        }
    );
}

#[tokio::test]
async fn task_working_directory_routes_shell_for_teammate() {
    let command = shell_pwd_command();
    let input = command_input_json(&command);
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "pwd", "shell", &input),
            text_stream(&model.id, "done"),
        ],
    );
    let provider_handle = provider.clone();

    let repo_dir = temp_team_dir("teammate-working-dir");
    let working_dir = repo_dir.join("alice-task");
    fs::create_dir_all(&working_dir).expect("create working dir");
    let team_dir = temp_team_dir("teammate-context-team");
    let tasks_dir = repo_dir.join(".tasks");
    let store = temp_store("teammate-working-dir");
    create_task_with_directory(
        &store,
        &tasks_dir,
        "Implement feature",
        "alice",
        vec![],
        Some("alice-task"),
    );

    let runtime = Runtime::builder()
        .with_store(store.clone())
        .with_policy(RuntimePolicy::permissive())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model,
            AgentConfig {
                team: team_config(team_dir),
                task: TaskConfig {
                    tasks_dir: tasks_dir.clone(),
                    reminder_threshold: 3,
                },
                workspace: workspace_config(&repo_dir),
                ..Default::default()
            },
        )
        .expect("spawn lead");

    lead.spawn_teammate(
        "alice",
        "coder",
        Some("Set up the task and verify cwd.".to_string()),
    )
    .await
    .expect("spawn teammate");
    wait_for_recorded_requests(&provider_handle, 2).await;
    wait_for_teammate_status(&lead, TeamMemberStatus::Idle).await;

    let requests = provider_handle.recorded_requests().await;
    assert!(request_contains_tool_result(
        &requests[1],
        working_dir.to_string_lossy().as_ref()
    ));
    assert_eq!(
        load_task(&store, &tasks_dir, 1)["workingDirectory"].as_str(),
        Some("alice-task")
    );
}

#[tokio::test]
async fn teammate_shell_without_working_directory_uses_base_dir() {
    let command = shell_pwd_command();
    let input = command_input_json(&command);
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "pwd", "shell", &input),
            text_stream(&model.id, "handled"),
        ],
    );
    let provider_handle = provider.clone();

    let repo_dir = temp_team_dir("missing-working-dir");
    let team_dir = temp_team_dir("missing-context-team");
    let tasks_dir = repo_dir.join(".tasks");
    let store = temp_store("missing-working-dir");
    create_task(&store, &tasks_dir, "Implement feature", "alice", vec![]);

    let runtime = Runtime::builder()
        .with_store(store)
        .with_policy(RuntimePolicy::permissive())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model,
            AgentConfig {
                team: team_config(team_dir),
                task: TaskConfig {
                    tasks_dir,
                    reminder_threshold: 3,
                },
                workspace: workspace_config(&repo_dir),
                ..Default::default()
            },
        )
        .expect("spawn lead");

    lead.spawn_teammate("alice", "coder", Some("Try to run pwd.".to_string()))
        .await
        .expect("spawn teammate");
    wait_for_recorded_requests(&provider_handle, 2).await;
    wait_for_teammate_status(&lead, TeamMemberStatus::Idle).await;

    let requests = provider_handle.recorded_requests().await;
    assert!(request_contains_tool_result(
        &requests[1],
        repo_dir.to_string_lossy().as_ref()
    ));
}

#[tokio::test]
async fn shell_working_directory_overrides_default_routing() {
    let command = shell_pwd_command();
    let input = command_input_with_working_directory_json(&command, "custom");
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "pwd", "shell", &input),
            text_stream(&model.id, "done"),
        ],
    );
    let provider_handle = provider.clone();

    let repo_dir = temp_team_dir("explicit-working-dir");
    let working_dir = repo_dir.join("custom");
    fs::create_dir_all(&working_dir).expect("create working dir");
    let runtime = Runtime::builder()
        .with_policy(RuntimePolicy::permissive())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "lead",
            model,
            AgentConfig {
                workspace: workspace_config(&repo_dir),
                task: TaskConfig {
                    tasks_dir: repo_dir.join(".tasks"),
                    reminder_threshold: 3,
                },
                ..Default::default()
            },
        )
        .expect("spawn agent");

    agent
        .send(vec![ContentBlock::Text {
            text: "Create a context and inspect it.".to_string(),
        }])
        .await
        .expect("send");

    let requests = provider_handle.recorded_requests().await;
    assert!(request_contains_tool_result(
        &requests[1],
        working_dir.to_string_lossy().as_ref()
    ));
}

#[tokio::test]
async fn shell_tool_is_denied_by_default_policy_and_audited() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "pwd", "shell", r#"{"command":"pwd"}"#),
            text_stream(&model.id, "done"),
        ],
    );
    let store = temp_store("policy-audit");
    let store_path = store.path().to_path_buf();
    let runtime = Runtime::builder()
        .with_store(store)
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    agent
        .send(vec![ContentBlock::Text {
            text: "try shell".to_string(),
        }])
        .await
        .expect("send");

    let conn = rusqlite::Connection::open(store_path).expect("open audit db");
    let payload: String = conn
        .query_row(
            "SELECT payload_json FROM audit_events WHERE event_type = 'authorization_denied' ORDER BY created_at DESC, id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("load audit payload");
    assert!(payload.contains("\"action\":\"shell_command\""));
    assert!(payload.contains("\"agent_id\":\"agent"));
}

#[tokio::test]
async fn files_tool_reads_numbered_lines() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "files-read",
                "files",
                r#"{"operations":[{"op":"read","path":"note.txt","offset":2,"limit":1}]}"#,
            ),
            text_stream(&model.id, "done"),
        ],
    );

    let repo_dir = temp_team_dir("files-read");
    fs::write(repo_dir.join("note.txt"), "alpha\nbeta\ngamma\n").expect("write note");
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                workspace: workspace_config(&repo_dir),
                ..Default::default()
            },
        )
        .expect("spawn agent");

    agent
        .send(vec![ContentBlock::Text {
            text: "read the second line".to_string(),
        }])
        .await
        .expect("send");

    assert_eq!(
        agent.history()[2],
        Message::user(ContentBlock::ToolResult {
            tool_use_id: "files-read".to_string(),
            content: "read note.txt\nL2: beta".into(),
            is_error: false,
        })
    );
}

#[tokio::test]
async fn files_tool_can_stage_create_then_read_in_one_call() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "files-create-read",
                "files",
                r#"{"operations":[{"op":"create","path":"draft.txt","content":"hello"},{"op":"read","path":"draft.txt"}]}"#,
            ),
            text_stream(&model.id, "done"),
        ],
    );

    let repo_dir = temp_team_dir("files-create-read");
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                workspace: workspace_config(&repo_dir),
                ..Default::default()
            },
        )
        .expect("spawn agent");

    agent
        .send(vec![ContentBlock::Text {
            text: "create a draft and read it back".to_string(),
        }])
        .await
        .expect("send");

    let content = match &agent.history()[2] {
        Message {
            role: Role::User,
            content,
        } => content.first().expect("tool result"),
        _ => panic!("expected tool result"),
    };
    match content {
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            assert_eq!(tool_use_id, "files-create-read");
            assert!(!is_error);
            assert!(content.contains("create draft.txt"));
            assert!(content.contains("read draft.txt"));
            assert!(content.contains("L1: hello"));
        }
        other => panic!("unexpected content block: {other:?}"),
    }
    assert_eq!(
        fs::read_to_string(repo_dir.join("draft.txt")).expect("read draft"),
        "hello"
    );
}

#[tokio::test]
async fn files_tool_can_update_existing_files() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "files-set-read",
                "files",
                r#"{"operations":[{"op":"set","path":"note.txt","content":"updated\n"},{"op":"read","path":"note.txt"}]}"#,
            ),
            text_stream(&model.id, "done"),
        ],
    );

    let repo_dir = temp_team_dir("files-set-read");
    fs::write(repo_dir.join("note.txt"), "original\n").expect("write note");
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                workspace: workspace_config(&repo_dir),
                ..Default::default()
            },
        )
        .expect("spawn agent");

    agent
        .send(vec![ContentBlock::Text {
            text: "update the note and read it back".to_string(),
        }])
        .await
        .expect("send");

    let content = match &agent.history()[2] {
        Message {
            role: Role::User,
            content,
        } => content.first().expect("tool result"),
        _ => panic!("expected tool result"),
    };
    match content {
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            assert_eq!(tool_use_id, "files-set-read");
            assert!(!is_error);
            assert!(content.contains("set note.txt"));
            assert!(content.contains("L1: updated"));
        }
        other => panic!("unexpected content block: {other:?}"),
    }
    assert_eq!(
        fs::read_to_string(repo_dir.join("note.txt")).expect("read note"),
        "updated\n"
    );
}

#[tokio::test]
async fn files_tool_lists_and_searches_with_staged_state() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "files-list-search",
                "files",
                r#"{"operations":[{"op":"create","path":"src/new.txt","content":"beta\ncreated\n"},{"op":"list","path":".","depth":2,"limit":10},{"op":"search","path":".","pattern":"beta","limit":10}]}"#,
            ),
            text_stream(&model.id, "done"),
        ],
    );

    let repo_dir = temp_team_dir("files-list-search");
    fs::create_dir_all(repo_dir.join("src")).expect("create src");
    fs::write(repo_dir.join("note.txt"), "alpha\nbeta\n").expect("write note");
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                workspace: workspace_config(&repo_dir),
                ..Default::default()
            },
        )
        .expect("spawn agent");

    agent
        .send(vec![ContentBlock::Text {
            text: "create a file, then list and search".to_string(),
        }])
        .await
        .expect("send");

    let tool_output = match &agent.history()[2] {
        Message {
            role: Role::User,
            content,
        } => match content.first().expect("tool result") {
            ContentBlock::ToolResult { content, .. } => content.clone(),
            other => panic!("unexpected content block: {other:?}"),
        },
        _ => panic!("expected tool result"),
    };
    assert!(tool_output.contains("create src/new.txt"));
    assert!(tool_output.contains("[file] note.txt"));
    assert!(tool_output.contains("[dir] src"));
    assert!(tool_output.contains("[file] src/new.txt"));
    assert!(tool_output.contains("note.txt:2: beta"));
    assert!(tool_output.contains("src/new.txt:1: beta"));
}

#[tokio::test]
async fn files_tool_aborts_without_partial_mutation_on_validation_error() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "files-fail",
                "files",
                r#"{"operations":[{"op":"create","path":"draft.txt","content":"hello"},{"op":"replace","path":"draft.txt","old":"missing","new":"present"}]}"#,
            ),
            text_stream(&model.id, "handled"),
        ],
    );

    let repo_dir = temp_team_dir("files-no-partial");
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                workspace: workspace_config(&repo_dir),
                ..Default::default()
            },
        )
        .expect("spawn agent");

    agent
        .send(vec![ContentBlock::Text {
            text: "this edit should fail cleanly".to_string(),
        }])
        .await
        .expect("send");

    match &agent.history()[2] {
        Message {
            role: Role::User,
            content,
        } => match content.first().expect("tool result") {
            ContentBlock::ToolResult {
                is_error: true,
                content,
                ..
            } => assert!(content.contains("Expected 1 replacement(s)")),
            other => panic!("unexpected content block: {other:?}"),
        },
        _ => panic!("expected tool result"),
    }
    assert!(!repo_dir.join("draft.txt").exists());
}

#[tokio::test]
async fn files_tool_denies_writes_outside_workspace_roots() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "files-denied",
                "files",
                r#"{"operations":[{"op":"create","path":"../outside.txt","content":"nope"}]}"#,
            ),
            text_stream(&model.id, "handled"),
        ],
    );

    let repo_dir = temp_team_dir("files-denied");
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                workspace: workspace_config(&repo_dir),
                ..Default::default()
            },
        )
        .expect("spawn agent");

    agent
        .send(vec![ContentBlock::Text {
            text: "try to write outside the repo".to_string(),
        }])
        .await
        .expect("send");

    match &agent.history()[2] {
        Message {
            role: Role::User,
            content,
        } => match content.first().expect("tool result") {
            ContentBlock::ToolResult {
                is_error: true,
                content,
                ..
            } => assert!(content.contains("outside the runtime policy write roots")),
            other => panic!("unexpected content block: {other:?}"),
        },
        _ => panic!("expected tool result"),
    }
    assert!(!repo_dir.join("..").join("outside.txt").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn files_tool_search_handles_symlink_loops() {
    use std::os::unix::fs::symlink;

    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "files-search-loop",
                "files",
                r#"{"operations":[{"op":"search","path":".","pattern":"never-match","limit":10}]}"#,
            ),
            text_stream(&model.id, "done"),
        ],
    );

    let repo_dir = temp_team_dir("files-search-loop");
    fs::write(repo_dir.join("note.txt"), "alpha\nbeta\n").expect("write note");
    symlink(".", repo_dir.join("loop")).expect("create loop symlink");

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                workspace: workspace_config(&repo_dir),
                ..Default::default()
            },
        )
        .expect("spawn agent");

    timeout(
        Duration::from_secs(2),
        agent.send(vec![ContentBlock::Text {
            text: "search the repo".to_string(),
        }]),
    )
    .await
    .expect("search should finish")
    .expect("send");

    match &agent.history()[2] {
        Message {
            role: Role::User,
            content,
        } => match content.first().expect("tool result") {
            ContentBlock::ToolResult {
                is_error: false,
                content,
                ..
            } => assert!(content.contains("(no matches)")),
            other => panic!("unexpected content block: {other:?}"),
        },
        _ => panic!("expected tool result"),
    }
}

#[tokio::test]
async fn run_options_tool_budget_blocks_second_tool_call() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![multi_tool_use_stream(
            &model.id,
            &[
                (
                    "read-1",
                    "files",
                    r#"{"operations":[{"op":"read","path":"note.txt"}]}"#,
                ),
                (
                    "read-2",
                    "files",
                    r#"{"operations":[{"op":"read","path":"note.txt"}]}"#,
                ),
            ],
        )],
    );

    let repo_dir = temp_team_dir("tool-budget");
    fs::write(repo_dir.join("note.txt"), "alpha\nbeta\n").expect("write note");
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                workspace: workspace_config(&repo_dir),
                ..Default::default()
            },
        )
        .expect("spawn agent");

    let error = agent
        .run(
            vec![ContentBlock::Text {
                text: "read the file twice".to_string(),
            }],
            RunOptions {
                tool_budget: Some(1),
                ..RunOptions::default()
            },
        )
        .await
        .expect_err("tool budget should fail");

    assert!(matches!(error, RuntimeError::ToolBudgetExceeded(1)));
    assert!(agent.history().is_empty());
}

#[tokio::test]
async fn parallel_tools_run_concurrently_and_preserve_history_order() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            multi_tool_use_stream(
                &model.id,
                &[
                    ("call-1", "probe_one", r#"{}"#),
                    ("call-2", "probe_two", r#"{}"#),
                ],
            ),
            text_stream(&model.id, "done"),
        ],
    );
    let log = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(ProbeTool::new(
            "probe_one",
            true,
            Duration::from_millis(40),
            Arc::clone(&log),
            Arc::clone(&active),
            Arc::clone(&max_active),
        ))
        .with_tool(ProbeTool::new(
            "probe_two",
            true,
            Duration::from_millis(40),
            Arc::clone(&log),
            Arc::clone(&active),
            Arc::clone(&max_active),
        ))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    agent
        .send(vec![ContentBlock::text("run the probes")])
        .await
        .expect("send");

    assert!(max_active.load(Ordering::SeqCst) >= 2);
    let tool_result_ids = agent
        .history()
        .iter()
        .filter(|message| message.role == Role::User)
        .flat_map(|message| {
            message.content.iter().filter_map(|block| match block {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                _ => None,
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        tool_result_ids,
        vec!["call-1".to_string(), "call-2".to_string()]
    );
}

#[tokio::test]
async fn parallel_batches_respect_exclusive_barriers() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            multi_tool_use_stream(
                &model.id,
                &[
                    ("call-1", "probe_one", r#"{}"#),
                    ("call-2", "probe_two", r#"{}"#),
                    ("call-3", "exclusive_probe", r#"{}"#),
                    ("call-4", "probe_three", r#"{}"#),
                ],
            ),
            text_stream(&model.id, "done"),
        ],
    );
    let log = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(ProbeTool::new(
            "probe_one",
            true,
            Duration::from_millis(30),
            Arc::clone(&log),
            Arc::clone(&active),
            Arc::clone(&max_active),
        ))
        .with_tool(ProbeTool::new(
            "probe_two",
            true,
            Duration::from_millis(30),
            Arc::clone(&log),
            Arc::clone(&active),
            Arc::clone(&max_active),
        ))
        .with_tool(ProbeTool::new(
            "exclusive_probe",
            false,
            Duration::from_millis(5),
            Arc::clone(&log),
            Arc::clone(&active),
            Arc::clone(&max_active),
        ))
        .with_tool(ProbeTool::new(
            "probe_three",
            true,
            Duration::from_millis(30),
            Arc::clone(&log),
            Arc::clone(&active),
            Arc::clone(&max_active),
        ))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    agent
        .send(vec![ContentBlock::text("run probes with barrier")])
        .await
        .expect("send");

    let log = log.lock().await.clone();
    let exclusive_start = log
        .iter()
        .position(|entry| entry == "exclusive_probe:start")
        .expect("exclusive start");
    let probe_one_start = log
        .iter()
        .position(|entry| entry == "probe_one:start")
        .expect("probe one start");
    let probe_two_start = log
        .iter()
        .position(|entry| entry == "probe_two:start")
        .expect("probe two start");
    let probe_three_start = log
        .iter()
        .position(|entry| entry == "probe_three:start")
        .expect("probe three start");

    assert!(probe_one_start < exclusive_start);
    assert!(probe_two_start < exclusive_start);
    assert!(exclusive_start < probe_three_start);
    assert!(max_active.load(Ordering::SeqCst) >= 2);
}

#[tokio::test]
async fn cancellation_during_parallel_batch_rolls_back_run() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![multi_tool_use_stream(
            &model.id,
            &[
                ("call-1", "probe_one", r#"{}"#),
                ("call-2", "probe_two", r#"{}"#),
            ],
        )],
    );
    let log = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(ProbeTool::new(
            "probe_one",
            true,
            Duration::from_millis(150),
            Arc::clone(&log),
            Arc::clone(&active),
            Arc::clone(&max_active),
        ))
        .with_tool(ProbeTool::new(
            "probe_two",
            true,
            Duration::from_millis(150),
            Arc::clone(&log),
            Arc::clone(&active),
            Arc::clone(&max_active),
        ))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");
    let cancellation = CancellationToken::default();
    let cancellation_clone = cancellation.clone();
    tokio::spawn(async move {
        sleep(Duration::from_millis(20)).await;
        cancellation_clone.cancel();
    });

    let error = agent
        .run(
            vec![ContentBlock::text("cancel while probes run")],
            RunOptions {
                cancellation: Some(cancellation),
                ..Default::default()
            },
        )
        .await
        .expect_err("run should cancel");

    assert!(matches!(error, RuntimeError::Cancelled));
    assert!(agent.history().is_empty());
}

#[tokio::test]
async fn run_options_model_budget_blocks_follow_up_round() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "read-1",
                "files",
                r#"{"operations":[{"op":"read","path":"note.txt"}]}"#,
            ),
            text_stream(&model.id, "done"),
        ],
    );

    let repo_dir = temp_team_dir("model-budget");
    fs::write(repo_dir.join("note.txt"), "alpha\nbeta\n").expect("write note");
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                workspace: workspace_config(&repo_dir),
                ..Default::default()
            },
        )
        .expect("spawn agent");

    let error = agent
        .run(
            vec![ContentBlock::Text {
                text: "read the file".to_string(),
            }],
            RunOptions {
                model_budget: Some(1),
                ..Default::default()
            },
        )
        .await
        .expect_err("model budget should fail");

    assert!(matches!(error, RuntimeError::ModelBudgetExceeded(1)));
    assert!(agent.history().is_empty());
}

#[tokio::test]
async fn run_options_cancelled_run_stops_before_provider_request() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![text_stream(&model.id, "done")],
    );
    let provider_handle = provider.clone();
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");
    let cancellation = CancellationToken::default();
    cancellation.cancel();

    let error = agent
        .run(
            vec![ContentBlock::Text {
                text: "stop".to_string(),
            }],
            RunOptions {
                cancellation: Some(cancellation),
                ..Default::default()
            },
        )
        .await
        .expect_err("cancellation should fail");

    assert!(matches!(error, RuntimeError::Cancelled));
    assert!(provider_handle.recorded_requests().await.is_empty());
    assert!(agent.history().is_empty());
}

#[tokio::test]
async fn completed_background_results_are_batched_in_completion_order() {
    let first_command = background_success_command("first", 20);
    let second_command = background_success_command("second", 50);
    let first_input = command_input_json(&first_command);
    let second_input = command_input_json(&second_command);
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            multi_tool_use_stream(
                &model.id,
                &[
                    ("tool-bg-1", "background_run", first_input.as_str()),
                    ("tool-bg-2", "background_run", second_input.as_str()),
                ],
            ),
            text_stream(&model.id, "continued"),
            text_stream(&model.id, "next turn"),
        ],
    );
    let provider_handle = provider.clone();

    let runtime = Runtime::builder()
        .with_store(temp_store("bg-results-batched-order"))
        .with_policy(RuntimePolicy::permissive())
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
    let command = background_failure_command("boom", 7, 50);
    let input = command_input_json(&command);
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "tool-bg", "background_run", &input),
            text_stream(&model.id, "continued"),
            text_stream(&model.id, "next turn"),
        ],
    );
    let provider_handle = provider.clone();

    let runtime = Runtime::builder()
        .with_store(temp_store("bg-failure-results"))
        .with_policy(RuntimePolicy::permissive())
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
    let command = background_success_command("bg-done", 50);
    let input = command_input_json(&command);
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "tool-bg", "background_run", &input),
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
        .with_store(temp_store("bg-requeue-failed-run"))
        .with_policy(RuntimePolicy::permissive())
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
    let model = model_info("model", BuiltinProvider::Anthropic);

    let default_provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
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
    assert!(default_tools.contains("shell"));
    assert!(default_tools.contains("background_run"));
    assert!(default_tools.contains("check_background"));
    assert!(default_tools.contains("compact"));
    assert!(default_tools.contains("memory_search"));
    assert!(default_tools.contains("memory_pin"));
    assert!(default_tools.contains("memory_forget"));
    assert!(default_tools.contains("files"));
    assert!(!default_tools.contains("read_file"));
    assert!(default_tools.contains("task"));
    assert!(default_tools.contains("task_create"));
    assert!(default_tools.contains("task_claim"));
    assert!(default_tools.contains("task_update"));
    assert!(default_tools.contains("task_list"));
    assert!(default_tools.contains("task_get"));
    assert!(!default_tools.contains("load_skill"));

    let empty_provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
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
    assert!(!empty_tools.contains("files"));
    assert!(!empty_tools.contains("compact"));
    assert!(!empty_tools.contains("memory_search"));
    assert!(!empty_tools.contains("memory_pin"));
    assert!(!empty_tools.contains("memory_forget"));
    assert!(!empty_tools.contains("task"));
    assert!(!empty_tools.contains("task_create"));
    assert!(!empty_tools.contains("task_claim"));
    assert!(!empty_tools.contains("task_update"));
    assert!(!empty_tools.contains("task_list"));
    assert!(!empty_tools.contains("task_get"));
    assert!(!empty_tools.contains("load_skill"));
}

#[tokio::test]
async fn tool_profile_only_exposes_requested_tools() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![text_stream(&model.id, "ok")],
    );
    let provider_handle = provider.clone();

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("echo_tool", "echoed"))
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                tool_profile: ToolProfile::only(["files", "echo_tool"]),
                ..Default::default()
            },
        )
        .unwrap();

    agent.send(vec![ContentBlock::text("hello")]).await.unwrap();

    let requests = provider_handle.recorded_requests().await;
    let tools = tool_names(&requests[0]);
    assert!(tools.contains("files"));
    assert!(tools.contains("echo_tool"));
    assert!(!tools.contains("shell"));
    assert!(!tools.contains("task"));
    assert!(!tools.contains("background_run"));
}

#[tokio::test]
async fn tool_profile_hide_blocks_named_tools() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![text_stream(&model.id, "ok")],
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
                tool_profile: ToolProfile::hide(["shell", "files"]),
                ..Default::default()
            },
        )
        .unwrap();

    agent.send(vec![ContentBlock::text("hello")]).await.unwrap();

    let requests = provider_handle.recorded_requests().await;
    let tools = tool_names(&requests[0]);
    assert!(!tools.contains("shell"));
    assert!(!tools.contains("files"));
    assert!(tools.contains("task"));
    assert!(tools.contains("memory_search"));
}

#[tokio::test]
async fn hidden_tool_profile_tool_choice_falls_back_to_auto() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![text_stream(&model.id, "ok")],
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
                    name: "shell".to_string(),
                }),
                tool_profile: ToolProfile::hide(["shell"]),
                ..Default::default()
            },
        )
        .unwrap();

    agent.send(vec![ContentBlock::text("hello")]).await.unwrap();

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests[0].tool_choice, Some(ToolChoice::Auto));
    assert!(!tool_names(&requests[0]).contains("shell"));
}

#[tokio::test]
async fn hidden_tool_profile_filters_deferred_tools_before_provider_request() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![text_stream(&model.id, "ok")],
    );
    let provider_handle = provider.clone();

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::deferred_success("deferred_tool", "echoed"))
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                tool_profile: ToolProfile::hide(["deferred_tool"]),
                provider_request_options: crate::provider::ProviderRequestOptions {
                    tool_search_mode: ToolSearchMode::Hosted,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();

    agent.send(vec![ContentBlock::text("hello")]).await.unwrap();

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(
        requests[0].provider_request_options.tool_search_mode,
        ToolSearchMode::Hosted
    );
    assert!(!tool_names(&requests[0]).contains("deferred_tool"));
}

#[tokio::test]
async fn deferred_tool_choice_reaches_provider_request_unchanged() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![text_stream(&model.id, "ok")],
    );
    let provider_handle = provider.clone();

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::deferred_success("deferred_tool", "echoed"))
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                tool_choice: Some(ToolChoice::Tool {
                    name: "deferred_tool".to_string(),
                }),
                provider_request_options: crate::provider::ProviderRequestOptions {
                    tool_search_mode: ToolSearchMode::Hosted,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();

    agent.send(vec![ContentBlock::text("hello")]).await.unwrap();

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(
        requests[0].tool_choice,
        Some(ToolChoice::Tool {
            name: "deferred_tool".to_string(),
        })
    );
    let deferred_tool = requests[0]
        .tools
        .iter()
        .find(|tool| tool.name == "deferred_tool")
        .expect("deferred tool present");
    assert_eq!(
        deferred_tool.loading_policy,
        crate::tool::ToolLoadingPolicy::Deferred
    );
    assert_eq!(
        requests[0].provider_request_options.tool_search_mode,
        ToolSearchMode::Hosted
    );
}

#[tokio::test]
async fn memory_search_tool_returns_provenance_fields() {
    let store = hybrid_temp_store("memory-search-tool");
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "memory-search-1",
                "memory_search",
                r#"{"query":"short answers","limit":5}"#,
            ),
            text_stream(&model.id, "searched"),
        ],
    );
    let runtime = Runtime::builder()
        .with_store(store.clone())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");
    let agent_id = agent.id().to_string();
    store
        .upsert_records(&[MemoryRecord {
            record_id: "fact:search:1".to_string(),
            agent_id: agent_id.clone(),
            kind: MemoryRecordKind::Fact,
            content: "The user likes short answers.".to_string(),
            source_revision: 1,
            created_at: 1,
            metadata_json: "{}".to_string(),
            source: Some("manual_pin".to_string()),
            pinned: true,
            score: None,
        }])
        .expect("seed records");

    agent
        .send(vec![ContentBlock::Text {
            text: "Search memory.".to_string(),
        }])
        .await
        .expect("run");

    let result = match &agent.history()[2].content[0] {
        ContentBlock::ToolResult { content, .. } => content.as_str(),
        other => panic!("expected tool result, got {other:?}"),
    };
    assert!(result.contains("\"source\": \"manual_pin\""));
    assert!(result.contains("\"why_retrieved\":"));
}

#[tokio::test]
async fn memory_forget_tool_rejects_cross_agent_record_ids() {
    let store = hybrid_temp_store("memory-forget-cross-agent");
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "memory-forget-cross-agent",
                "memory_forget",
                r#"{"record_id":"fact:shared:1"}"#,
            ),
            text_stream(&model.id, "handled"),
        ],
    );
    let runtime = Runtime::builder()
        .with_store(store.clone())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let owner = runtime
        .spawn_with_config(
            "owner",
            model.clone(),
            AgentConfig {
                memory: MemoryConfig {
                    write_tools_enabled: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .expect("spawn owner");
    let owner_id = owner.id().to_string();
    let mut other = runtime
        .spawn_with_config(
            "other",
            model,
            AgentConfig {
                memory: MemoryConfig {
                    write_tools_enabled: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .expect("spawn other");

    store
        .upsert_records(&[MemoryRecord {
            record_id: "fact:shared:1".to_string(),
            agent_id: owner_id.clone(),
            kind: MemoryRecordKind::Fact,
            content: "Owner memory only".to_string(),
            source_revision: 1,
            created_at: 1,
            metadata_json: "{}".to_string(),
            source: Some("manual_pin".to_string()),
            pinned: true,
            score: None,
        }])
        .expect("seed records");

    other
        .send(vec![ContentBlock::Text {
            text: "Forget that.".to_string(),
        }])
        .await
        .expect("run");

    let result = match &other.history()[2].content[0] {
        ContentBlock::ToolResult { content, .. } => content.as_str(),
        other => panic!("expected tool result, got {other:?}"),
    };
    assert!(result.contains("was not found for this agent"));
    let records = store
        .search_records(&owner_id, "Owner memory", 10)
        .expect("search records");
    assert_eq!(records.len(), 1);
}

#[tokio::test]
async fn registered_skills_are_exposed_and_load_skill_returns_wrapped_content() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
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
                ..Default::default()
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
        Message::user(ContentBlock::ToolResult {
            tool_use_id: "tool-skill".to_string(),
            content: "<skill name=\"git\">\nUse feature branches.\nRun tests first.\n</skill>"
                .into(),
            is_error: false,
        })
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
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
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
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
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

    assert_eq!(agent.history().len(), 6);
    assert!(agent.history().iter().any(|message| {
        *message
            == Message::user(ContentBlock::ToolResult {
                tool_use_id: "tool-parent".to_string(),
                content: "child summary".into(),
                is_error: false,
            })
    }));
    assert_eq!(
        agent.last_message(),
        Some(&Message::assistant(ContentBlock::text("parent done")))
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
    assert!(child_tools.contains("shell"));
    assert!(child_tools.contains("files"));
    assert!(!child_tools.contains("idle"));
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
async fn task_subagent_inherits_tool_profile_and_internal_task_hide() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
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
                tool_profile: ToolProfile::only(["task", "shell", "files"]),
                ..Default::default()
            },
        )
        .unwrap();

    agent
        .send(vec![ContentBlock::text("delegate")])
        .await
        .unwrap();

    let requests = provider_handle.recorded_requests().await;
    let parent_tools = tool_names(&requests[0]);
    assert!(parent_tools.contains("task"));
    assert!(parent_tools.contains("shell"));
    assert!(parent_tools.contains("files"));
    assert!(!parent_tools.contains("background_run"));

    let child_tools = tool_names(&requests[1]);
    assert!(child_tools.contains("shell"));
    assert!(child_tools.contains("files"));
    assert!(!child_tools.contains("task"));
    assert!(!child_tools.contains("background_run"));
}

#[tokio::test]
async fn task_subagent_does_not_force_hidden_task_tool_choice() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
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
                ..Default::default()
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
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
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

    assert!(agent.history().iter().any(|message| {
        *message
            == Message::user(ContentBlock::ToolResult {
                tool_use_id: "tool-parent".to_string(),
                content: "Subagent failed: failed to stream provider response: malformed provider stream: boom"
                    .into(),
                is_error: true,
            })
    }));
    assert_eq!(
        agent.last_message(),
        Some(&Message::assistant(ContentBlock::text("handled")))
    );

    let subagents = agent.watch_snapshot().borrow().subagents.clone();
    assert_eq!(subagents.len(), 1);
    assert!(matches!(
        &subagents[0].status,
        SpawnedAgentStatus::Failed(message)
            if message == "failed to stream provider response: malformed provider stream: boom"
    ));
}

#[tokio::test]
async fn child_rejects_nested_task_requests_without_recursing() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
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

    assert!(agent.history().iter().any(|message| {
        *message
            == Message::user(ContentBlock::ToolResult {
                tool_use_id: "parent-task".to_string(),
                content: "child recovered".into(),
                is_error: false,
            })
    }));

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 4);
    assert!(!tool_names(&requests[1]).contains("task"));
    assert_eq!(requests[2].messages.len(), 3);
    assert_eq!(
        requests[2].messages[2],
        Message::user(ContentBlock::ToolResult {
            tool_use_id: "child-task".to_string(),
            content: "Tool 'task' is not available for this agent".into(),
            is_error: true,
        })
    );
}

#[tokio::test]
async fn task_tool_returns_error_when_child_hits_round_limit() {
    let model = model_info("model", BuiltinProvider::Anthropic);
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

    let provider = ScriptedProvider::new(BuiltinProvider::Anthropic, vec![model.clone()], scripts);
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

    assert!(agent.history().iter().any(|message| {
        *message
            == Message::user(ContentBlock::ToolResult {
                tool_use_id: "parent-task".to_string(),
                content: "Subagent failed: max rounds exceeded at 30".into(),
                is_error: true,
            })
    }));
    assert_eq!(
        agent.last_message(),
        Some(&Message::assistant(ContentBlock::text("parent handled")))
    );
    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 32);
}

#[tokio::test]
async fn team_spawn_tool_registers_persistent_teammate() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "team-spawn",
                "team_spawn",
                r#"{"name":"alice","role":"researcher"}"#,
            ),
            text_stream(&model.id, "team ready"),
        ],
    );
    let provider_handle = provider.clone();

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "lead",
            model,
            AgentConfig {
                team: team_config(temp_team_dir("spawn-tool")),
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = agent.subscribe_events();

    agent
        .send(vec![ContentBlock::Text {
            text: "build a team".to_string(),
        }])
        .await
        .unwrap();

    assert!(matches!(
        &agent.history()[2].content[0],
        ContentBlock::ToolResult { content, is_error: false, .. }
            if content.contains("Spawned persistent teammate 'alice'")
    ));

    let teammates = agent.watch_snapshot().borrow().teammates.clone();
    assert_eq!(teammates.len(), 1);
    assert_eq!(teammates[0].name, "alice");
    assert_eq!(teammates[0].role, "researcher");
    assert_eq!(teammates[0].status, TeamMemberStatus::Idle);

    let requests = provider_handle.recorded_requests().await;
    assert!(tool_names(&requests[0]).contains("team_spawn"));
    assert!(tool_names(&requests[0]).contains("team_send"));
    assert!(tool_names(&requests[0]).contains("team_broadcast"));
    assert!(tool_names(&requests[0]).contains("team_read_inbox"));
    assert!(tool_names(&requests[0]).contains("team_request"));
    assert!(tool_names(&requests[0]).contains("team_respond"));
    assert!(tool_names(&requests[0]).contains("team_list_requests"));

    let events = collect_events(&mut events);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::TeammateSpawned { teammate } if teammate.name == "alice"))
    );
}

#[tokio::test]
async fn persistent_teammate_processes_mail_and_reports_back_to_lead() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "child-send",
                "team_send",
                r#"{"to":"lead","content":"investigation complete"}"#,
            ),
            text_stream(&model.id, "done"),
            text_stream(&model.id, "thanks"),
        ],
    );
    let provider_handle = provider.clone();

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model,
            AgentConfig {
                team: team_config(temp_team_dir("mailbox")),
                ..Default::default()
            },
        )
        .unwrap();

    lead.spawn_teammate("alice", "researcher", None)
        .await
        .expect("spawn teammate");
    lead.send_team_message("alice", "Check the task graph")
        .expect("send message");

    wait_for_recorded_requests(&provider_handle, 2).await;
    wait_for_teammate_status(&lead, TeamMemberStatus::Idle).await;

    lead.send(vec![ContentBlock::Text {
        text: "status?".to_string(),
    }])
    .await
    .unwrap();

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 3);
    let child_tools = tool_names(&requests[0]);
    assert!(child_tools.contains("team_send"));
    assert!(child_tools.contains("team_read_inbox"));
    assert!(child_tools.contains("team_request"));
    assert!(child_tools.contains("team_respond"));
    assert!(child_tools.contains("team_list_requests"));
    assert!(child_tools.contains("idle"));
    assert!(!child_tools.contains("team_spawn"));
    assert!(!child_tools.contains("team_broadcast"));
    assert!(!child_tools.contains("task_create"));
    assert!(child_tools.contains("task_claim"));
    assert!(child_tools.contains("task_update"));
    assert!(child_tools.contains("task_list"));
    assert!(child_tools.contains("task_get"));

    let inbox = latest_team_inbox_text(&requests[2]).expect("team inbox");
    assert!(inbox.contains("alice"));
    assert!(inbox.contains("investigation complete"));

    let teammates = lead.watch_snapshot().borrow().teammates.clone();
    assert_eq!(teammates.len(), 1);
    assert_eq!(teammates[0].status, TeamMemberStatus::Idle);
}

#[tokio::test]
async fn teammate_inherits_tool_profile_and_internal_team_hides() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "child-send",
                "team_send",
                r#"{"to":"lead","content":"done"}"#,
            ),
            text_stream(&model.id, "done"),
        ],
    );
    let provider_handle = provider.clone();

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model,
            AgentConfig {
                team: team_config(temp_team_dir("tool-profile-team")),
                tool_profile: ToolProfile::only([
                    "team_spawn",
                    "team_send",
                    "team_read_inbox",
                    "team_request",
                    "team_respond",
                    "team_list_requests",
                    "idle",
                    "task_create",
                    "task_claim",
                    "task_update",
                    "task_list",
                    "task_get",
                ]),
                ..Default::default()
            },
        )
        .unwrap();

    lead.spawn_teammate("alice", "researcher", None)
        .await
        .expect("spawn teammate");
    lead.send_team_message("alice", "Check the task graph")
        .expect("send message");

    wait_for_recorded_requests(&provider_handle, 1).await;
    wait_for_teammate_status(&lead, TeamMemberStatus::Idle).await;

    let requests = provider_handle.recorded_requests().await;
    let child_tools = tool_names(&requests[0]);
    assert!(child_tools.contains("team_send"));
    assert!(child_tools.contains("team_read_inbox"));
    assert!(child_tools.contains("team_request"));
    assert!(child_tools.contains("team_respond"));
    assert!(child_tools.contains("team_list_requests"));
    assert!(child_tools.contains("idle"));
    assert!(child_tools.contains("task_claim"));
    assert!(child_tools.contains("task_update"));
    assert!(child_tools.contains("task_list"));
    assert!(child_tools.contains("task_get"));
    assert!(!child_tools.contains("team_spawn"));
    assert!(!child_tools.contains("task_create"));
    assert!(!child_tools.contains("team_broadcast"));
}

#[tokio::test]
async fn broadcast_tool_sends_to_every_other_known_agent() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "broadcast-tool",
                "team_broadcast",
                r#"{"content":"team sync at noon"}"#,
            ),
            text_stream(&model.id, "broadcasted"),
        ],
    );

    let team_dir = temp_team_dir("broadcast-tool");
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model.clone(),
            AgentConfig {
                team: team_config(team_dir.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    let alice = runtime
        .spawn_with_config(
            "alice",
            model.clone(),
            AgentConfig {
                team: team_config(team_dir.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    let bob = runtime
        .spawn_with_config(
            "bob",
            model,
            AgentConfig {
                team: team_config(team_dir),
                ..Default::default()
            },
        )
        .unwrap();

    lead.send(vec![ContentBlock::Text {
        text: "tell everyone about the sync".to_string(),
    }])
    .await
    .unwrap();

    let alice_inbox = alice.read_team_inbox().expect("alice inbox");
    let bob_inbox = bob.read_team_inbox().expect("bob inbox");
    let lead_inbox = lead.read_team_inbox().expect("lead inbox");

    assert_eq!(alice_inbox.len(), 1);
    assert_eq!(alice_inbox[0].sender, "lead");
    assert_eq!(alice_inbox[0].content, "team sync at noon");
    assert_eq!(alice_inbox[0].kind, TeamMessageKind::Broadcast);

    assert_eq!(bob_inbox.len(), 1);
    assert_eq!(bob_inbox[0].sender, "lead");
    assert_eq!(bob_inbox[0].content, "team sync at noon");
    assert_eq!(bob_inbox[0].kind, TeamMessageKind::Broadcast);

    assert!(lead_inbox.is_empty());

    let tool_result = lead
        .history()
        .iter()
        .flat_map(|message| message.content.iter())
        .find_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .expect("broadcast tool result");
    assert!(tool_result.contains("2 recipient(s)"));
    assert!(tool_result.contains("alice"));
    assert!(tool_result.contains("bob"));
}

#[tokio::test]
async fn teammate_message_updates_lead_unread_count_before_next_turn() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "child-send",
                "team_send",
                r#"{"to":"lead","content":"plan is ready"}"#,
            ),
            text_stream(&model.id, "done"),
        ],
    );
    let provider_handle = provider.clone();

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model,
            AgentConfig {
                team: team_config(temp_team_dir("lead-unread-message")),
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = lead.subscribe_events();

    lead.spawn_teammate(
        "alice",
        "researcher",
        Some("Send me an update.".to_string()),
    )
    .await
    .expect("spawn teammate");

    wait_for_recorded_requests(&provider_handle, 2).await;
    wait_for_pending_team_messages(&lead, 1).await;

    assert_eq!(lead.watch_snapshot().borrow().pending_team_messages, 1);
    assert_eq!(provider_handle.recorded_requests().await.len(), 2);

    let events = collect_events(&mut events);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::TeamInboxUpdated { unread_count } if *unread_count == 1
    )));
}

#[tokio::test]
async fn protocol_messages_update_lead_unread_count_and_clear_on_drain() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "bob-plan",
                "team_request",
                r#"{"to":"lead","protocol":"plan_approval","content":"risky refactor plan"}"#,
            ),
            text_stream(&model.id, "waiting"),
            text_stream(&model.id, "lead handled it"),
            text_stream(&model.id, "done waiting"),
        ],
    );
    let provider_handle = provider.clone();

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model,
            AgentConfig {
                team: team_config(temp_team_dir("lead-unread-protocol")),
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = lead.subscribe_events();

    lead.spawn_teammate(
        "bob",
        "refactorer",
        Some("Send me a plan request.".to_string()),
    )
    .await
    .expect("spawn teammate");

    wait_for_recorded_requests(&provider_handle, 2).await;
    wait_for_pending_team_messages(&lead, 1).await;

    let request_id = lead.watch_snapshot().borrow().protocol_requests[0]
        .request_id
        .clone();
    lead.respond_team_protocol(&request_id, false, Some("too risky".to_string()))
        .expect("reject plan");

    lead.send(vec![ContentBlock::Text {
        text: "review inbox".to_string(),
    }])
    .await
    .unwrap();

    assert_eq!(lead.watch_snapshot().borrow().pending_team_messages, 0);

    let events = collect_events(&mut events);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::TeamInboxUpdated { unread_count } if *unread_count == 1
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::TeamInboxUpdated { unread_count } if *unread_count == 0
    )));
}

#[tokio::test]
async fn team_request_tool_persists_pending_request_and_updates_snapshot() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "team-request",
                "team_request",
                r#"{"to":"lead","protocol":"shutdown","content":"Please shut down gracefully."}"#,
            ),
            text_stream(&model.id, "request queued"),
        ],
    );

    let team_dir = temp_team_dir("protocol-request-tool");
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "lead",
            model,
            AgentConfig {
                team: team_config(team_dir.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = agent.subscribe_events();

    agent
        .send(vec![ContentBlock::Text {
            text: "queue a shutdown request".to_string(),
        }])
        .await
        .unwrap();

    let requests = agent.watch_snapshot().borrow().protocol_requests.clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].protocol, "shutdown");
    assert_eq!(requests[0].status, TeamProtocolStatus::Pending);
    assert_eq!(requests[0].to, "lead");

    let events = collect_events(&mut events);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::TeamProtocolRequested { request }
            if request.protocol == "shutdown" && request.status == TeamProtocolStatus::Pending
    )));
}

#[tokio::test]
async fn team_respond_tool_resolves_request_and_sends_correlated_response() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let team_dir = temp_team_dir("protocol-respond-tool");
    let store = temp_store("protocol-respond-tool");
    let runtime = Runtime::builder()
        .with_store(store.clone())
        .with_provider_instance(ScriptedProvider::new(
            BuiltinProvider::Anthropic,
            vec![model.clone()],
            vec![],
        ))
        .build()
        .expect("build runtime");
    let _lead = runtime
        .spawn_with_config(
            "lead",
            model.clone(),
            AgentConfig {
                team: team_config(team_dir.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    let requester = runtime
        .spawn_with_config(
            "reviewer",
            model.clone(),
            AgentConfig {
                team: team_config(team_dir.clone()),
                ..Default::default()
            },
        )
        .unwrap();

    let request = requester
        .request_team_protocol("lead", "plan_approval", "risky refactor plan")
        .expect("create request");
    let request_id = request.request_id.clone();
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "team-respond",
                "team_respond",
                &format!(r#"{{"request_id":"{request_id}","approve":true,"reason":"looks good"}}"#),
            ),
            text_stream(&model.id, "approved"),
        ],
    );
    let provider_handle = provider.clone();

    let runtime = Runtime::builder()
        .with_store(store)
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model.clone(),
            AgentConfig {
                team: team_config(team_dir.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    let requester = runtime
        .spawn_with_config(
            "reviewer",
            model,
            AgentConfig {
                team: team_config(team_dir.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = lead.subscribe_events();

    lead.send(vec![ContentBlock::Text {
        text: "review the plan".to_string(),
    }])
    .await
    .unwrap();

    wait_for_recorded_requests(&provider_handle, 2).await;

    let protocol_requests = lead.watch_snapshot().borrow().protocol_requests.clone();
    assert_eq!(protocol_requests.len(), 1);
    assert_eq!(protocol_requests[0].status, TeamProtocolStatus::Approved);
    assert_eq!(
        protocol_requests[0].resolution_reason.as_deref(),
        Some("looks good")
    );

    let inbox = requester.read_team_inbox().expect("reviewer inbox");
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].kind, TeamMessageKind::Response);
    assert_eq!(inbox[0].request_id.as_deref(), Some(request_id.as_str()));
    assert_eq!(inbox[0].approve, Some(true));

    let events = collect_events(&mut events);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::TeamProtocolResolved { request }
            if request.request_id == request_id.as_str()
    )));
}

#[tokio::test]
async fn team_list_requests_tool_filters_visible_requests() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "team-list",
                "team_list_requests",
                r#"{"status":"pending","protocol":"plan_approval","direction":"inbound"}"#,
            ),
            text_stream(&model.id, "listed"),
        ],
    );

    let team_dir = temp_team_dir("protocol-list-tool");
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model.clone(),
            AgentConfig {
                team: team_config(team_dir.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    let reviewer = runtime
        .spawn_with_config(
            "reviewer",
            model.clone(),
            AgentConfig {
                team: team_config(team_dir.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    let architect = runtime
        .spawn_with_config(
            "architect",
            model,
            AgentConfig {
                team: team_config(team_dir.clone()),
                ..Default::default()
            },
        )
        .unwrap();

    let pending = reviewer
        .request_team_protocol("lead", "plan_approval", "plan A")
        .expect("pending request");
    let resolved = architect
        .request_team_protocol("lead", "shutdown", "stop")
        .expect("resolved request");
    lead.respond_team_protocol(&resolved.request_id, false, Some("not now".to_string()))
        .expect("resolve request");

    lead.send(vec![ContentBlock::Text {
        text: "list pending reviews".to_string(),
    }])
    .await
    .unwrap();

    let tool_result = lead
        .history()
        .iter()
        .flat_map(|message| message.content.iter())
        .find_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.to_display_string()),
            _ => None,
        })
        .expect("team_list_requests tool result");
    let listed: serde_json::Value = serde_json::from_str(&tool_result).expect("parse tool output");
    let listed = listed.as_array().expect("array");
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0]["request_id"].as_str(),
        Some(pending.request_id.as_str())
    );
}

#[tokio::test]
async fn plan_approval_request_response_keeps_teammate_alive() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "bob-plan",
                "team_request",
                r#"{"to":"lead","protocol":"plan_approval","content":"risky refactor plan"}"#,
            ),
            text_stream(&model.id, "waiting for review"),
            text_stream(&model.id, "plan rejected, continuing safely"),
            text_stream(&model.id, "still available"),
        ],
    );
    let provider_handle = provider.clone();

    let team_dir = temp_team_dir("plan-approval");
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model,
            AgentConfig {
                team: team_config(team_dir.clone()),
                ..Default::default()
            },
        )
        .unwrap();

    lead.spawn_teammate(
        "bob",
        "refactorer",
        Some("Propose your plan first.".to_string()),
    )
    .await
    .expect("spawn teammate");

    wait_for_recorded_requests(&provider_handle, 2).await;
    let requests = lead.watch_snapshot().borrow().protocol_requests.clone();
    assert_eq!(requests.len(), 1);
    let request_id = requests[0].request_id.clone();
    assert_eq!(requests[0].status, TeamProtocolStatus::Pending);

    lead.respond_team_protocol(&request_id, false, Some("too risky".to_string()))
        .expect("respond to plan");

    wait_for_recorded_requests(&provider_handle, 3).await;
    wait_for_teammate_status(&lead, TeamMemberStatus::Idle).await;

    let requests = lead.watch_snapshot().borrow().protocol_requests.clone();
    assert_eq!(requests[0].status, TeamProtocolStatus::Rejected);
    assert_eq!(requests[0].resolution_reason.as_deref(), Some("too risky"));
}

#[tokio::test]
async fn shutdown_approval_shuts_down_teammate_after_current_wake_cycle() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let (stream, tx) = controlled_stream();
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![stream, text_stream(&model.id, "shutting down now")],
    );
    let provider_handle = provider.clone();

    let team_dir = temp_team_dir("shutdown-protocol");
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model,
            AgentConfig {
                team: team_config(team_dir.clone()),
                ..Default::default()
            },
        )
        .unwrap();

    lead.spawn_teammate("alice", "coder", None)
        .await
        .expect("spawn teammate");

    let request = lead
        .request_team_protocol("alice", "shutdown", "Please stop after this turn.")
        .expect("shutdown request");

    tx.send(Ok(ProviderEvent::MessageStarted {
        id: "msg-shutdown".to_string(),
        model: "model".to_string(),
        role: Role::Assistant,
    }))
    .expect("message start");
    tx.send(Ok(ProviderEvent::ContentBlockStarted {
        index: 0,
        kind: ContentBlockStart::ToolUse {
            id: "shutdown-response".to_string(),
            name: "team_respond".to_string(),
        },
    }))
    .expect("tool start");
    tx.send(Ok(ProviderEvent::ContentBlockDelta {
        index: 0,
        delta: ContentBlockDelta::ToolUseInputJson(format!(
            r#"{{"request_id":"{}","approve":true,"reason":"wrapping up"}}"#,
            request.request_id
        )),
    }))
    .expect("tool delta");
    tx.send(Ok(ProviderEvent::ContentBlockStopped { index: 0 }))
        .expect("tool stop");
    tx.send(Ok(ProviderEvent::MessageStopped))
        .expect("message stop");
    drop(tx);

    wait_for_recorded_requests(&provider_handle, 2).await;
    wait_for_teammate_status(&lead, TeamMemberStatus::Shutdown).await;

    let requests = lead.watch_snapshot().borrow().protocol_requests.clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].status, TeamProtocolStatus::Approved);
    assert_eq!(
        requests[0].resolution_reason.as_deref(),
        Some("wrapping up")
    );

    let teammates = lead.watch_snapshot().borrow().teammates.clone();
    assert_eq!(teammates[0].status, TeamMemberStatus::Shutdown);
}

#[tokio::test]
async fn failed_run_requeues_protocol_messages_and_preserves_request_state() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![erroring_stream(
            vec![
                ProviderEvent::MessageStarted {
                    id: "msg-error".to_string(),
                    model: model.id.clone(),
                    role: Role::Assistant,
                },
                ProviderEvent::ContentBlockStarted {
                    index: 0,
                    kind: ContentBlockStart::Text,
                },
                ProviderEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::Text("starting".to_string()),
                },
            ],
            ProviderError::InvalidResponse("boom".to_string()),
        )],
    );

    let team_dir = temp_team_dir("protocol-requeue");
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model.clone(),
            AgentConfig {
                team: team_config(team_dir.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    let reviewer = runtime
        .spawn_with_config(
            "reviewer",
            model,
            AgentConfig {
                team: team_config(team_dir.clone()),
                ..Default::default()
            },
        )
        .unwrap();

    let request = reviewer
        .request_team_protocol("lead", "plan_approval", "please review")
        .expect("create request");

    let error = lead
        .send(vec![ContentBlock::Text {
            text: "handle inbox".to_string(),
        }])
        .await
        .expect_err("run should fail");
    assert!(matches!(
        error,
        crate::error::RuntimeError::FailedToStreamResponse(_)
    ));
    assert_eq!(lead.watch_snapshot().borrow().pending_team_messages, 1);

    let inbox = lead.read_team_inbox().expect("requeued inbox");
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].kind, TeamMessageKind::Request);
    assert_eq!(
        inbox[0].request_id.as_deref(),
        Some(request.request_id.as_str())
    );

    let requests = lead.watch_snapshot().borrow().protocol_requests.clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].status, TeamProtocolStatus::Pending);
}

#[tokio::test]
async fn failed_teammate_can_recover_on_next_wake() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            erroring_stream(
                vec![ProviderEvent::MessageStarted {
                    id: "msg-error".to_string(),
                    model: model.id.clone(),
                    role: Role::Assistant,
                }],
                ProviderError::InvalidResponse("boom".to_string()),
            ),
            text_stream(&model.id, "recovered"),
        ],
    );
    let provider_handle = provider.clone();

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model,
            AgentConfig {
                team: team_config(temp_team_dir("teammate-recover")),
                ..Default::default()
            },
        )
        .unwrap();

    lead.spawn_teammate("alice", "researcher", Some("first try".to_string()))
        .await
        .expect("spawn teammate");
    wait_for_teammate_status(
        &lead,
        TeamMemberStatus::Failed(
            "failed to stream provider response: invalid provider response: boom".to_string(),
        ),
    )
    .await;

    lead.send_team_message("alice", "try again")
        .expect("send retry");
    wait_for_recorded_requests(&provider_handle, 2).await;
    wait_for_teammate_status(&lead, TeamMemberStatus::Idle).await;
}

#[tokio::test]
async fn persisted_protocol_requests_load_on_restart() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(BuiltinProvider::Anthropic, vec![model.clone()], vec![]);

    let team_dir = temp_team_dir("protocol-restart");
    let store = temp_store("protocol-restart");
    let runtime = Runtime::builder()
        .with_store(store.clone())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let lead = runtime
        .spawn_with_config(
            "lead",
            model.clone(),
            AgentConfig {
                team: team_config(team_dir.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    let reviewer = runtime
        .spawn_with_config(
            "reviewer",
            model.clone(),
            AgentConfig {
                team: team_config(team_dir.clone()),
                ..Default::default()
            },
        )
        .unwrap();

    reviewer
        .request_team_protocol("lead", "plan_approval", "plan one")
        .expect("create request");

    let provider = ScriptedProvider::new(BuiltinProvider::Anthropic, vec![model.clone()], vec![]);
    let runtime = Runtime::builder()
        .with_store(store)
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let restarted = runtime
        .spawn_with_config(
            "lead",
            model,
            AgentConfig {
                team: team_config(team_dir),
                ..Default::default()
            },
        )
        .unwrap();

    assert_eq!(lead.watch_snapshot().borrow().protocol_requests.len(), 1);
    assert_eq!(
        restarted.watch_snapshot().borrow().protocol_requests.len(),
        1
    );
    assert_eq!(
        restarted.watch_snapshot().borrow().protocol_requests[0].status,
        TeamProtocolStatus::Pending
    );
    assert_eq!(restarted.watch_snapshot().borrow().pending_team_messages, 1);
}

#[tokio::test]
async fn persisted_teammates_reload_as_shutdown_without_live_actor() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(BuiltinProvider::Anthropic, vec![model.clone()], vec![]);

    let team_dir = temp_team_dir("teammate-restart-status");
    let store = temp_store("teammate-restart-status");
    let runtime = Runtime::builder()
        .with_store(store.clone())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model.clone(),
            AgentConfig {
                team: team_config(team_dir.clone()),
                ..Default::default()
            },
        )
        .unwrap();

    lead.spawn_teammate("alice", "researcher", None)
        .await
        .expect("spawn teammate");

    let provider = ScriptedProvider::new(BuiltinProvider::Anthropic, vec![model.clone()], vec![]);
    let runtime = Runtime::builder()
        .with_store(store)
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let restarted = runtime
        .spawn_with_config(
            "lead",
            model,
            AgentConfig {
                team: team_config(team_dir),
                ..Default::default()
            },
        )
        .unwrap();

    let teammates = restarted.watch_snapshot().borrow().teammates.clone();
    assert_eq!(teammates.len(), 1);
    assert_eq!(teammates[0].name, "alice");
    assert_eq!(teammates[0].status, TeamMemberStatus::Idle);
}

#[tokio::test]
async fn team_spawn_revives_shutdown_teammate_name_after_restart() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(BuiltinProvider::Anthropic, vec![model.clone()], vec![]);

    let team_dir = temp_team_dir("teammate-revive");
    let store = temp_store("teammate-revive");
    let runtime = Runtime::builder()
        .with_store(store.clone())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model.clone(),
            AgentConfig {
                team: team_config(team_dir.clone()),
                ..Default::default()
            },
        )
        .unwrap();

    lead.spawn_teammate("bob", "researcher", None)
        .await
        .expect("spawn teammate");

    let provider = ScriptedProvider::new(BuiltinProvider::Anthropic, vec![model.clone()], vec![]);
    let runtime = Runtime::builder()
        .with_store(store)
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut restarted = runtime
        .spawn_with_config(
            "lead",
            model,
            AgentConfig {
                team: team_config(team_dir.clone()),
                ..Default::default()
            },
        )
        .unwrap();

    restarted
        .spawn_teammate("bob", "refactor specialist", None)
        .await
        .expect("revive teammate");

    let teammates = restarted.watch_snapshot().borrow().teammates.clone();
    assert_eq!(teammates.len(), 1);
    assert_eq!(teammates[0].name, "bob");
    assert_eq!(teammates[0].role, "refactor specialist");
    assert_eq!(teammates[0].status, TeamMemberStatus::Idle);
}

#[tokio::test]
async fn autonomous_teammate_auto_claims_ready_task_after_spawn() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![text_stream(&model.id, "claimed work")],
    );
    let provider_handle = provider.clone();

    let team_dir = temp_team_dir("auto-claim-team");
    let tasks_dir = temp_team_dir("auto-claim-tasks");
    let store = temp_store("auto-claim");
    create_task(&store, &tasks_dir, "Implement feature", "", vec![]);

    let runtime = Runtime::builder()
        .with_store(store.clone())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model,
            AgentConfig {
                team: autonomous_team_config(
                    team_dir,
                    Duration::from_millis(10),
                    Duration::from_millis(120),
                ),
                task: TaskConfig {
                    tasks_dir: tasks_dir.clone(),
                    reminder_threshold: 3,
                },
                ..Default::default()
            },
        )
        .unwrap();

    lead.spawn_teammate("alice", "coder", None)
        .await
        .expect("spawn teammate");

    wait_for_recorded_requests(&provider_handle, 1).await;
    wait_for_teammate_status(&lead, TeamMemberStatus::Idle).await;
    wait_for_snapshot_task_owner(&lead, 1, "alice").await;

    let task = load_task(&store, &tasks_dir, 1);
    assert_eq!(task["owner"].as_str(), Some("alice"));
    assert_eq!(lead.watch_snapshot().borrow().tasks[0].owner, "alice");

    let requests = provider_handle.recorded_requests().await;
    let auto_claim = latest_auto_claim_text(&requests[0]).expect("auto-claim text");
    assert!(auto_claim.contains("Task #1"));
    assert!(auto_claim.contains("Implement feature"));
    assert!(request_contains_text(
        &requests[0],
        "<reminder>Update your task status."
    ));
}

#[tokio::test]
async fn autonomous_teammates_do_not_double_claim_same_task() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![text_stream(&model.id, "picked it up")],
    );
    let provider_handle = provider.clone();

    let team_dir = temp_team_dir("claim-race-team");
    let tasks_dir = temp_team_dir("claim-race-tasks");
    let store = temp_store("claim-race");
    create_task(&store, &tasks_dir, "One task", "", vec![]);

    let runtime = Runtime::builder()
        .with_store(store.clone())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model,
            AgentConfig {
                team: autonomous_team_config(
                    team_dir,
                    Duration::from_millis(10),
                    Duration::from_millis(70),
                ),
                task: TaskConfig {
                    tasks_dir: tasks_dir.clone(),
                    reminder_threshold: 3,
                },
                ..Default::default()
            },
        )
        .unwrap();

    lead.spawn_teammate("alice", "coder", None)
        .await
        .expect("spawn alice");
    lead.spawn_teammate("bob", "coder", None)
        .await
        .expect("spawn bob");

    wait_for_recorded_requests(&provider_handle, 1).await;
    sleep(Duration::from_millis(120)).await;

    let task = load_task(&store, &tasks_dir, 1);
    let owner = task["owner"].as_str().expect("owner");
    assert!(owner == "alice" || owner == "bob");
    assert_eq!(provider_handle.recorded_requests().await.len(), 1);
}

#[tokio::test]
async fn autonomous_teammate_does_not_claim_more_work_while_owning_unfinished_task() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![text_stream(&model.id, "looking into task 1")],
    );
    let provider_handle = provider.clone();

    let team_dir = temp_team_dir("claim-owned-work-team");
    let tasks_dir = temp_team_dir("claim-owned-work-tasks");
    let store = temp_store("claim-owned-work");
    create_task(&store, &tasks_dir, "Task one", "", vec![]);
    create_task(&store, &tasks_dir, "Task two", "", vec![]);

    let runtime = Runtime::builder()
        .with_store(store.clone())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model,
            AgentConfig {
                team: autonomous_team_config(
                    team_dir,
                    Duration::from_millis(10),
                    Duration::from_millis(80),
                ),
                task: TaskConfig {
                    tasks_dir: tasks_dir.clone(),
                    reminder_threshold: 3,
                },
                ..Default::default()
            },
        )
        .unwrap();

    lead.spawn_teammate("alice", "coder", None)
        .await
        .expect("spawn teammate");

    wait_for_recorded_requests(&provider_handle, 1).await;
    wait_for_snapshot_task_owner(&lead, 1, "alice").await;
    sleep(Duration::from_millis(120)).await;

    assert_eq!(
        load_task(&store, &tasks_dir, 1)["owner"].as_str(),
        Some("alice")
    );
    assert_eq!(load_task(&store, &tasks_dir, 2)["owner"].as_str(), Some(""));
    assert_eq!(provider_handle.recorded_requests().await.len(), 1);
}

#[tokio::test]
async fn autonomous_teammate_claims_task_after_dependency_unblocks() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![text_stream(&model.id, "starting unblocked task")],
    );

    let team_dir = temp_team_dir("unblock-team");
    let tasks_dir = temp_team_dir("unblock-tasks");
    let store = temp_store("unblock");
    create_task(&store, &tasks_dir, "Blocked elsewhere", "lead", vec![]);
    create_task(&store, &tasks_dir, "Ready later", "", vec![1]);

    let runtime = Runtime::builder()
        .with_store(store.clone())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model,
            AgentConfig {
                team: autonomous_team_config(
                    team_dir,
                    Duration::from_millis(10),
                    Duration::from_millis(300),
                ),
                task: TaskConfig {
                    tasks_dir: tasks_dir.clone(),
                    reminder_threshold: 3,
                },
                ..Default::default()
            },
        )
        .unwrap();

    lead.spawn_teammate("alice", "coder", None)
        .await
        .expect("spawn teammate");

    sleep(Duration::from_millis(30)).await;
    assert_eq!(load_task(&store, &tasks_dir, 2)["owner"].as_str(), Some(""));

    task::execute_with_store(
        &store,
        &TaskIntrinsicTool::Update,
        json!({"taskId": 1, "status": "completed"}),
        tasks_dir.as_path(),
        TaskAccess::Lead,
    )
    .expect("complete blocker");

    wait_for_task_owner(&store, &tasks_dir, 2, "alice").await;
}

#[tokio::test]
async fn teammate_task_updates_are_limited_to_owned_tasks() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "own-task",
                "task_update",
                r#"{"taskId":1,"status":"in_progress"}"#,
            ),
            text_stream(&model.id, "updated own task"),
            tool_use_stream(
                &model.id,
                "other-task",
                "task_update",
                r#"{"taskId":2,"status":"completed"}"#,
            ),
            text_stream(&model.id, "could not touch task 2"),
        ],
    );
    let team_dir = temp_team_dir("owned-update-team");
    let tasks_dir = temp_team_dir("owned-update-tasks");
    let store = temp_store("owned-update");
    create_task(&store, &tasks_dir, "Alice task", "alice", vec![]);
    create_task(&store, &tasks_dir, "Shared task", "", vec![]);

    let runtime = Runtime::builder()
        .with_store(store.clone())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model,
            AgentConfig {
                team: team_config(team_dir),
                task: TaskConfig {
                    tasks_dir: tasks_dir.clone(),
                    reminder_threshold: 3,
                },
                ..Default::default()
            },
        )
        .unwrap();

    lead.spawn_teammate("alice", "coder", Some("Start task 1.".to_string()))
        .await
        .expect("spawn teammate");
    wait_for_teammate_status(&lead, TeamMemberStatus::Idle).await;

    lead.send_team_message("alice", "Now try to complete task 2.")
        .expect("send message");
    sleep(Duration::from_millis(100)).await;
    wait_for_teammate_status(&lead, TeamMemberStatus::Idle).await;

    assert_eq!(
        load_task(&store, &tasks_dir, 1)["status"].as_str(),
        Some("in_progress")
    );
    assert_eq!(
        load_task(&store, &tasks_dir, 2)["status"].as_str(),
        Some("pending")
    );
}

#[tokio::test]
async fn teammate_task_subagent_inherits_owner_restrictions() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "delegate-task",
                "task",
                r#"{"prompt":"finish the other task"}"#,
            ),
            tool_use_stream(
                &model.id,
                "illegal-update",
                "task_update",
                r#"{"taskId":2,"status":"completed"}"#,
            ),
            text_stream(&model.id, "child could not change task 2"),
            text_stream(&model.id, "delegation handled"),
        ],
    );
    let team_dir = temp_team_dir("teammate-task-subagent");
    let tasks_dir = temp_team_dir("teammate-task-subagent-tasks");
    let store = temp_store("teammate-task-subagent");
    create_task(&store, &tasks_dir, "Alice task", "alice", vec![]);
    create_task(&store, &tasks_dir, "Bob task", "bob", vec![]);

    let runtime = Runtime::builder()
        .with_store(store.clone())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model,
            AgentConfig {
                team: team_config(team_dir),
                task: TaskConfig {
                    tasks_dir: tasks_dir.clone(),
                    reminder_threshold: 3,
                },
                ..Default::default()
            },
        )
        .unwrap();

    lead.spawn_teammate("alice", "coder", Some("delegate it".to_string()))
        .await
        .expect("spawn teammate");
    wait_for_teammate_status(&lead, TeamMemberStatus::Idle).await;

    assert_eq!(
        load_task(&store, &tasks_dir, 2)["status"].as_str(),
        Some("pending")
    );
}

#[tokio::test]
async fn idle_tool_returns_teammate_to_idle() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![tool_use_stream(&model.id, "idle-now", "idle", "{}")],
    );
    let provider_handle = provider.clone();

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model,
            AgentConfig {
                team: team_config(temp_team_dir("idle-tool-team")),
                ..Default::default()
            },
        )
        .unwrap();

    lead.spawn_teammate("alice", "coder", Some("Check in then idle.".to_string()))
        .await
        .expect("spawn teammate");

    wait_for_recorded_requests(&provider_handle, 1).await;
    wait_for_teammate_status(&lead, TeamMemberStatus::Idle).await;
    assert_eq!(provider_handle.recorded_requests().await.len(), 1);
}

#[tokio::test]
async fn autonomous_idle_timeout_shuts_down_and_same_name_can_respawn() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(BuiltinProvider::Anthropic, vec![model.clone()], vec![]);

    let team_dir = temp_team_dir("idle-timeout-team");
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model.clone(),
            AgentConfig {
                team: autonomous_team_config(
                    team_dir.clone(),
                    Duration::from_millis(10),
                    Duration::from_millis(40),
                ),
                ..Default::default()
            },
        )
        .unwrap();

    lead.spawn_teammate("alice", "researcher", None)
        .await
        .expect("spawn teammate");
    wait_for_teammate_status(&lead, TeamMemberStatus::Shutdown).await;

    lead.spawn_teammate("alice", "researcher", None)
        .await
        .expect("respawn teammate");
}

#[tokio::test]
async fn teammate_identity_is_reinjected_after_compaction() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            text_stream(&model.id, "first done"),
            text_stream(&model.id, "summary"),
            text_stream(&model.id, "second done"),
            text_stream(&model.id, "extra done"),
        ],
    );
    let provider_handle = provider.clone();

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model,
            AgentConfig {
                team: team_config(temp_team_dir("identity-compact-team")),
                compaction: crate::agent::CompactionConfig {
                    auto_compact_threshold_tokens: Some(1),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();

    lead.spawn_teammate("alice", "researcher", Some("first".to_string()))
        .await
        .expect("spawn teammate");
    wait_for_recorded_requests(&provider_handle, 1).await;
    wait_for_teammate_status(&lead, TeamMemberStatus::Idle).await;

    lead.send_team_message("alice", "second")
        .expect("send second");
    wait_for_recorded_requests(&provider_handle, 3).await;
    wait_for_teammate_status(&lead, TeamMemberStatus::Idle).await;

    let requests = provider_handle.recorded_requests().await;
    assert!(
        requests
            .iter()
            .skip(1)
            .any(|request| request_contains_text(request, "<identity>"))
    );
    assert!(
        requests
            .iter()
            .skip(1)
            .any(|request| request_contains_text(request, "I am alice"))
    );
}

fn collect_events(receiver: &mut tokio::sync::broadcast::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        events.push(event);
    }
    events
}

async fn wait_for_pending_team_messages(agent: &Agent, expected_count: usize) {
    for _ in 0..200 {
        if agent.watch_snapshot().borrow().pending_team_messages == expected_count {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }

    panic!("timed out waiting for {expected_count} pending team messages");
}

async fn wait_for_background_task_count(agent: &Agent, expected_count: usize) {
    for _ in 0..1000 {
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
    for _ in 0..1000 {
        let background_tasks = agent.watch_snapshot().borrow().background_tasks.clone();
        if background_tasks.len() == expected_count
            && background_tasks.iter().all(|task| task.status == status)
        {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }

    panic!("timed out waiting for {expected_count} background tasks to reach {status:?}");
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

fn latest_team_inbox_text<'a>(request: &'a Request<'a>) -> Option<&'a str> {
    request
        .messages
        .iter()
        .rev()
        .flat_map(|message| message.content.iter())
        .find_map(|block| match block {
            ContentBlock::Text { text } if text.contains("<team-inbox>") => Some(text.as_str()),
            _ => None,
        })
}

fn latest_auto_claim_text<'a>(request: &'a Request<'a>) -> Option<&'a str> {
    request
        .messages
        .iter()
        .rev()
        .flat_map(|message| message.content.iter())
        .find_map(|block| match block {
            ContentBlock::Text { text } if text.contains("<auto-claimed>") => Some(text.as_str()),
            _ => None,
        })
}

fn request_contains_text(request: &Request<'_>, pattern: &str) -> bool {
    request
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .any(|block| matches!(block, ContentBlock::Text { text } if text.contains(pattern)))
}

fn request_contains_tool_result(request: &Request<'_>, pattern: &str) -> bool {
    request
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .any(|block| {
            matches!(
                block,
                ContentBlock::ToolResult { content, .. } if content.contains(pattern)
            )
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

fn temp_team_dir(label: &str) -> PathBuf {
    let unique = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let path =
        std::env::temp_dir().join(format!("mentra-runtime-team-{label}-{timestamp}-{unique}"));
    fs::create_dir_all(&path).expect("create team dir");
    path
}

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

fn hybrid_temp_store(label: &str) -> HybridRuntimeStore {
    let unique = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let base_dir = std::env::temp_dir().join(format!(
        "mentra-runtime-hybrid-store-{label}-{timestamp}-{unique}"
    ));
    HybridRuntimeStore::with_memory_path(
        base_dir.join("runtime.sqlite"),
        base_dir.join("memory.sqlite"),
    )
}

fn team_config(team_dir: PathBuf) -> TeamConfig {
    TeamConfig {
        team_dir,
        ..Default::default()
    }
}

fn autonomous_team_config(
    team_dir: PathBuf,
    poll_interval: Duration,
    idle_timeout: Duration,
) -> TeamConfig {
    TeamConfig {
        team_dir,
        autonomy: TeamAutonomyConfig {
            enabled: true,
            poll_interval,
            idle_timeout,
        },
    }
}

fn create_task(
    store: &SqliteRuntimeStore,
    tasks_dir: &Path,
    subject: &str,
    owner: &str,
    blocked_by: Vec<u64>,
) {
    create_task_with_directory(store, tasks_dir, subject, owner, blocked_by, None);
}

fn create_task_with_directory(
    store: &SqliteRuntimeStore,
    tasks_dir: &Path,
    subject: &str,
    owner: &str,
    blocked_by: Vec<u64>,
    working_directory: Option<&str>,
) {
    task::execute_with_store(
        store,
        &TaskIntrinsicTool::Create,
        json!({
            "subject": subject,
            "owner": owner,
            "workingDirectory": working_directory,
            "blockedBy": blocked_by,
        }),
        tasks_dir,
        TaskAccess::Lead,
    )
    .expect("create task");
}

fn workspace_config(base_dir: &Path) -> WorkspaceConfig {
    WorkspaceConfig {
        base_dir: base_dir.to_path_buf(),
        ..Default::default()
    }
}

fn load_task(store: &SqliteRuntimeStore, tasks_dir: &Path, task_id: u64) -> serde_json::Value {
    serde_json::from_str(
        &task::execute_with_store(
            store,
            &TaskIntrinsicTool::Get,
            json!({ "taskId": task_id }),
            tasks_dir,
            TaskAccess::Lead,
        )
        .expect("load task"),
    )
    .expect("parse task")
}

fn write_skill(root: &Path, name: &str, content: &str) {
    let skill_dir = root.join(name);
    fs::create_dir_all(&skill_dir).expect("create skill dir");
    fs::write(skill_dir.join("SKILL.md"), content).expect("write skill");
}

async fn wait_for_recorded_requests(provider: &ScriptedProvider, expected: usize) {
    for _ in 0..500 {
        if provider.recorded_requests().await.len() >= expected {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }

    panic!("timed out waiting for {expected} recorded requests");
}

async fn wait_for_background_task_status(
    store: &SqliteRuntimeStore,
    agent_id: &str,
    task_id: &str,
    expected_status: BackgroundTaskStatus,
) {
    for _ in 0..500 {
        let tasks =
            <SqliteRuntimeStore as crate::background::BackgroundStore>::load_background_tasks(
                store, agent_id,
            )
            .expect("load background tasks");
        if tasks
            .iter()
            .any(|task| task.id == task_id && task.status == expected_status)
        {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }

    panic!("timed out waiting for background task {task_id} to reach {expected_status:?}");
}

async fn wait_for_background_task_record(
    store: &SqliteRuntimeStore,
    agent_id: &str,
    expected_count: usize,
) {
    for _ in 0..500 {
        let tasks =
            <SqliteRuntimeStore as crate::background::BackgroundStore>::load_background_tasks(
                store, agent_id,
            )
            .expect("load background tasks");
        if tasks.len() == expected_count {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }

    panic!("timed out waiting for {expected_count} background task records");
}

async fn wait_for_teammate_status(agent: &Agent, expected: TeamMemberStatus) {
    for _ in 0..500 {
        let teammates = agent.watch_snapshot().borrow().teammates.clone();
        if teammates.len() == 1 && teammates[0].status == expected {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }

    panic!("timed out waiting for teammate status {expected:?}");
}

async fn wait_for_task_owner(
    store: &SqliteRuntimeStore,
    tasks_dir: &Path,
    task_id: u64,
    owner: &str,
) {
    for _ in 0..500 {
        if load_task(store, tasks_dir, task_id)["owner"].as_str() == Some(owner) {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }

    panic!("timed out waiting for task {task_id} owner {owner}");
}

async fn wait_for_snapshot_task_owner(agent: &Agent, task_id: u64, owner: &str) {
    for _ in 0..200 {
        let tasks = agent.watch_snapshot().borrow().tasks.clone();
        if tasks
            .iter()
            .find(|task| task.id == task_id)
            .map(|task| task.owner.as_str())
            == Some(owner)
        {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }

    panic!("timed out waiting for snapshot task {task_id} owner {owner}");
}
