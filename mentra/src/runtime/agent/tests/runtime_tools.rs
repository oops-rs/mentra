use crate::{
    provider::model::{
        ContentBlock, ContentBlockDelta, ContentBlockStart, Message, ModelProviderKind,
        ProviderError, ProviderEvent, Request, Role, ToolChoice,
    },
    runtime::{AgentConfig, AgentEvent, Runtime, SpawnedAgentStatus},
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

    let mut runtime = Runtime::new_empty();
    runtime.register_provider_instance(provider);
    runtime.register_tool(StaticTool::success("echo_tool", "tool output"));
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

    let mut runtime = Runtime::new_empty();
    runtime.register_provider_instance(provider);
    runtime.register_tool(StaticTool::failure("failing_tool", "tool failed"));
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
async fn default_runtime_exposes_task_and_new_empty_does_not() {
    let model = model_info("model", ModelProviderKind::Anthropic);

    let default_provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![text_stream(&model.id, "ok")],
    );
    let default_handle = default_provider.clone();
    let mut default_runtime = Runtime::default();
    default_runtime.register_provider_instance(default_provider);
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
    assert!(default_tools.contains("read_file"));
    assert!(default_tools.contains("task"));

    let empty_provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
        vec![model.clone()],
        vec![text_stream(&model.id, "ok")],
    );
    let empty_handle = empty_provider.clone();
    let mut empty_runtime = Runtime::new_empty();
    empty_runtime.register_provider_instance(empty_provider);
    let mut empty_agent = empty_runtime.spawn("agent", model).unwrap();
    empty_agent
        .send(vec![ContentBlock::Text {
            text: "hello".to_string(),
        }])
        .await
        .unwrap();

    let empty_requests = empty_handle.recorded_requests().await;
    let empty_tools = tool_names(&empty_requests[0]);
    assert!(!empty_tools.contains("task"));
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

    let mut runtime = Runtime::default();
    runtime.register_provider_instance(provider);
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

    let mut runtime = Runtime::default();
    runtime.register_provider_instance(provider);
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

    let mut runtime = Runtime::default();
    runtime.register_provider_instance(provider);
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

    let mut runtime = Runtime::default();
    runtime.register_provider_instance(provider);
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

    let mut runtime = Runtime::default();
    runtime.register_provider_instance(provider);
    runtime.register_tool(StaticTool::success("echo_tool", "pong"));
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

fn tool_names(request: &Request<'_>) -> std::collections::HashSet<String> {
    request.tools.iter().map(|tool| tool.name.clone()).collect()
}
