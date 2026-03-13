use std::sync::{Arc, Mutex};

use crate::{
    BuiltinProvider, ContentBlock, Message, Role,
    agent::{AgentConfig, AgentEvent, AgentStatus},
    provider::{ContentBlockDelta, ContentBlockStart, ProviderError, ProviderEvent},
    runtime::{RunOptions, Runtime, RuntimeHook, RuntimeHookEvent, RuntimePolicy},
};

use super::support::{ScriptedProvider, StaticTool, erroring_stream, model_info, ok_stream};

#[tokio::test]
async fn send_streamed_text_turn_emits_events_and_commits_history() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
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
        Some(&Message::assistant(ContentBlock::text("Hello")))
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

#[tokio::test]
async fn run_respects_tool_budget() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![tool_use_stream(
            &model.id,
            "tool-1",
            "test_tool",
            r#"{"value":"hi"}"#,
        )],
    );

    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("test_tool", "ok"))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();

    let error = agent
        .run(
            vec![ContentBlock::Text {
                text: "hi".to_string(),
            }],
            RunOptions {
                tool_budget: Some(0),
                ..RunOptions::default()
            },
        )
        .await
        .expect_err("tool budget should abort run");

    assert!(matches!(
        error,
        crate::runtime::RuntimeError::ToolBudgetExceeded(0)
    ));
}

#[tokio::test]
async fn policy_can_deny_shell_tool_execution() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "pwd", "shell", r#"{"command":"pwd"}"#),
            text_stream("done"),
        ],
    );

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .with_policy(
            RuntimePolicy::permissive()
                .allow_shell_commands(false)
                .allow_background_commands(false),
        )
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "run pwd".to_string(),
        }])
        .await
        .expect("send");

    assert!(matches!(
        &agent.history()[2].content[0],
        ContentBlock::ToolResult { content, is_error: true, .. }
            if content.contains("disabled by the runtime policy")
    ));
}

#[tokio::test]
async fn approval_required_shell_emits_hook_and_returns_tool_error() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "tool-shell",
                "shell",
                r#"{"command":"python -c 'print(1)'","justification":"needed for validation"}"#,
            ),
            text_stream("done"),
        ],
    );
    let recorded = Arc::new(Mutex::new(Vec::new()));

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .with_policy(RuntimePolicy::default().allow_shell_commands(true))
        .with_hook(RecordingHook {
            events: recorded.clone(),
        })
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "run python".to_string(),
        }])
        .await
        .expect("send");

    assert!(matches!(
        &agent.history()[2].content[0],
        ContentBlock::ToolResult { content, is_error: true, .. }
            if content.contains("Command requires approval")
    ));

    let events = recorded.lock().expect("hook events poisoned").clone();
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeHookEvent::ShellApprovalRequired {
            tool_name,
            parsed_kind,
            justification,
            ..
        } if tool_name == "shell"
            && parsed_kind == "unknown"
            && justification.as_deref() == Some("needed for validation")
    )));
}

#[tokio::test]
async fn custom_hooks_observe_model_and_tool_execution() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "tool-1", "test_tool", r#"{"value":"hi"}"#),
            text_stream("done"),
        ],
    );
    let recorded = Arc::new(Mutex::new(Vec::new()));

    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("test_tool", "ok"))
        .with_hook(RecordingHook {
            events: recorded.clone(),
        })
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "hi".to_string(),
        }])
        .await
        .expect("send");

    let events = recorded.lock().expect("hook events poisoned").clone();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RuntimeHookEvent::ModelRequestStarted { .. }))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeHookEvent::ToolExecutionStarted { tool_name, .. } if tool_name == "test_tool"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeHookEvent::ToolExecutionFinished { tool_name, is_error: false, .. }
            if tool_name == "test_tool"
    )));
}

#[derive(Clone)]
struct RecordingHook {
    events: Arc<Mutex<Vec<RuntimeHookEvent>>>,
}

impl RuntimeHook for RecordingHook {
    fn on_event(
        &self,
        _store: &dyn crate::runtime::RuntimeStore,
        event: &RuntimeHookEvent,
    ) -> Result<(), crate::runtime::RuntimeError> {
        self.events
            .lock()
            .expect("hook events poisoned")
            .push(event.clone());
        Ok(())
    }
}

fn text_stream(text: &str) -> super::support::StreamScript {
    ok_stream(vec![
        ProviderEvent::MessageStarted {
            id: "msg-text".to_string(),
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

fn tool_use_stream(
    model: &str,
    id: &str,
    name: &str,
    input_json: &str,
) -> super::support::StreamScript {
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
