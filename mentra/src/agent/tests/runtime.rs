use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::time::sleep;

use crate::{
    BuiltinProvider, ContentBlock, Role,
    provider::{ContentBlockDelta, ContentBlockStart, ProviderError, ProviderEvent},
    runtime::{
        RunOptions, Runtime, RuntimeHook, RuntimeHookEvent, RuntimePolicy,
        is_transient_runtime_error,
    },
    tool::{ExecutableTool, ToolContext, ToolResult, ToolSpec},
};

use super::support::{ScriptedProvider, StaticTool, model_info, ok_stream};

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

#[tokio::test]
async fn tools_can_read_registered_app_context() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "tool-ctx", "app_context_tool", r#"{}"#),
            text_stream("done"),
        ],
    );
    let app_state = Arc::new(TestAppState {
        label: "configured",
    });

    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_context(app_state.clone())
        .with_tool(AppContextTool)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();

    assert_eq!(
        runtime
            .app_context::<TestAppState>()
            .expect("app context should be registered")
            .label,
        "configured"
    );

    agent
        .send(vec![ContentBlock::Text {
            text: "use the app_context_tool".to_string(),
        }])
        .await
        .expect("send");

    assert!(matches!(
        &agent.history()[2].content[0],
        ContentBlock::ToolResult { content, is_error: false, .. }
            if content == "configured"
    ));
}

#[tokio::test]
async fn tool_execution_timeout_returns_tool_error() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "tool-slow", "slow_tool", r#"{}"#),
            text_stream("done"),
        ],
    );

    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(SlowTool)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "run the slow tool".to_string(),
        }])
        .await
        .expect("send");

    assert!(matches!(
        &agent.history()[2].content[0],
        ContentBlock::ToolResult { content, is_error: true, .. }
            if content.contains("timed out after 20ms")
    ));
}

#[test]
fn transient_runtime_error_helper_matches_provider_retry_policy() {
    let transient = crate::runtime::RuntimeError::FailedToStreamResponse(ProviderError::Http {
        status: reqwest::StatusCode::TOO_MANY_REQUESTS,
        body: String::new(),
    });
    let permanent = crate::runtime::RuntimeError::FailedToStreamResponse(
        ProviderError::InvalidRequest("bad request".to_string()),
    );

    assert!(is_transient_runtime_error(&transient));
    assert!(!is_transient_runtime_error(&permanent));
    assert!(!is_transient_runtime_error(
        &crate::runtime::RuntimeError::EmptyAssistantResponse
    ));
}

#[derive(Debug)]
struct TestAppState {
    label: &'static str,
}

struct AppContextTool;

#[async_trait]
impl ExecutableTool for AppContextTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::builder("app_context_tool")
            .description("Return a value from the runtime app context.")
            .input_schema(json!({
                "type": "object",
                "properties": {}
            }))
            .build()
    }

    async fn execute_mut(&self, ctx: ToolContext<'_>, _input: Value) -> ToolResult {
        Ok(ctx.app_context::<TestAppState>()?.label.to_string())
    }
}

struct SlowTool;

#[async_trait]
impl ExecutableTool for SlowTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::builder("slow_tool")
            .description("Sleep long enough to trigger a timeout.")
            .input_schema(json!({
                "type": "object",
                "properties": {}
            }))
            .execution_timeout(Duration::from_millis(20))
            .build()
    }

    async fn execute_mut(&self, _ctx: ToolContext<'_>, _input: Value) -> ToolResult {
        sleep(Duration::from_millis(60)).await;
        Ok("finished".to_string())
    }
}

#[derive(Clone)]
struct RecordingHook {
    events: Arc<Mutex<Vec<RuntimeHookEvent>>>,
}

impl RuntimeHook for RecordingHook {
    fn on_event(
        &self,
        _store: &dyn crate::runtime::AuditStore,
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
