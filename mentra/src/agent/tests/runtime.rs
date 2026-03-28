use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::time::sleep;

use crate::{
    BuiltinProvider, ContentBlock, Role,
    provider::{ContentBlockDelta, ContentBlockStart, ProviderError, ProviderEvent, TokenUsage},
    runtime::{
        RunOptions, Runtime, RuntimeHook, RuntimeHookEvent, RuntimePolicy,
        is_transient_runtime_error,
    },
    tool::{
        ToolAuthorizationDecision, ToolAuthorizationOutcome, ToolAuthorizationRequest,
        ToolAuthorizer, ToolContext, ToolDefinition, ToolExecutor, ToolResult, ToolSpec,
    },
};

use super::support::{ScriptedProvider, StaticTool, erroring_stream, model_info, ok_stream};

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
async fn tool_authorizer_allows_tool_execution_and_captures_preview() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "tool-1", "test_tool", r#"{"value":"hi"}"#),
            text_stream("done"),
        ],
    );
    let requests = Arc::new(Mutex::new(Vec::new()));

    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("test_tool", "ok"))
        .with_tool_authorizer(RecordingAuthorizer::allow(requests.clone()))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "hi".to_string(),
        }])
        .await
        .expect("send");

    assert!(matches!(
        &agent.history()[2].content[0],
        ContentBlock::ToolResult { content, is_error: false, .. }
            if content.to_display_string() == "ok"
    ));

    let requests = requests.lock().expect("requests poisoned");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].tool_name, "test_tool");
    assert_eq!(
        requests[0].preview.structured_input,
        json!({ "value": "hi" })
    );
}

#[tokio::test]
async fn tool_authorizer_can_prompt_shell_tool_and_emit_hooks() {
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
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::new(Mutex::new(Vec::new()));

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .with_policy(RuntimePolicy::default().allow_shell_commands(true))
        .with_tool_authorizer(RecordingAuthorizer::prompt(
            "needs manual review",
            requests.clone(),
        ))
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
            if content.contains("Tool execution requires approval: needs manual review")
    ));

    let requests = requests.lock().expect("requests poisoned");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].tool_name, "shell");
    assert_eq!(
        requests[0].preview.structured_input["kind"].as_str(),
        Some("shell")
    );

    let events = recorded.lock().expect("hook events poisoned").clone();
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeHookEvent::ToolAuthorizationStarted { tool_name, .. } if tool_name == "shell"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeHookEvent::ToolAuthorizationFinished { tool_name, outcome, .. }
            if tool_name == "shell" && *outcome == ToolAuthorizationOutcome::Prompt
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeHookEvent::ToolAuthorizationBlocked { tool_name, outcome, .. }
            if tool_name == "shell" && *outcome == ToolAuthorizationOutcome::Prompt
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        RuntimeHookEvent::ToolExecutionStarted { tool_name, .. } if tool_name == "shell"
    )));
}

#[tokio::test]
async fn tool_authorizer_can_deny_background_run() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "tool-bg",
                "background_run",
                r#"{"command":"python -c 'print(1)'","justification":"background probe"}"#,
            ),
            text_stream("done"),
        ],
    );
    let requests = Arc::new(Mutex::new(Vec::new()));

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .with_policy(
            RuntimePolicy::default()
                .allow_shell_commands(true)
                .allow_background_commands(true),
        )
        .with_tool_authorizer(RecordingAuthorizer::deny(
            "background tasks require review",
            requests.clone(),
        ))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "run background work".to_string(),
        }])
        .await
        .expect("send");

    assert!(matches!(
        &agent.history()[2].content[0],
        ContentBlock::ToolResult { content, is_error: true, .. }
            if content.contains("Tool execution denied: background tasks require review")
    ));

    let requests = requests.lock().expect("requests poisoned");
    assert_eq!(
        requests[0].preview.structured_input["kind"].as_str(),
        Some("background_run")
    );
    assert_eq!(
        requests[0].preview.structured_input["background"].as_bool(),
        Some(true)
    );
}

#[tokio::test]
async fn tool_authorizer_errors_block_execution() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "tool-1", "test_tool", r#"{"value":"hi"}"#),
            text_stream("done"),
        ],
    );

    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("test_tool", "ok"))
        .with_tool_authorizer(RecordingAuthorizer::error("authorizer unavailable"))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "hi".to_string(),
        }])
        .await
        .expect("send");

    assert!(matches!(
        &agent.history()[2].content[0],
        ContentBlock::ToolResult { content, is_error: true, .. }
            if content.contains("authorizer unavailable")
    ));
}

#[tokio::test]
async fn tool_authorizer_timeout_blocks_execution() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "tool-1", "test_tool", r#"{"value":"hi"}"#),
            text_stream("done"),
        ],
    );

    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("test_tool", "ok"))
        .with_tool_authorizer(RecordingAuthorizer::delayed_allow(
            Duration::from_millis(50),
            Duration::from_millis(10),
        ))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "hi".to_string(),
        }])
        .await
        .expect("send");

    assert!(matches!(
        &agent.history()[2].content[0],
        ContentBlock::ToolResult { content, is_error: true, .. }
            if content.contains("authorizer timed out")
    ));
}

#[tokio::test]
async fn files_tool_authorization_preview_exposes_resolved_paths() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "files-read",
                "files",
                r#"{"operations":[{"op":"read","path":"README.md","offset":1,"limit":1}]}"#,
            ),
            text_stream("done"),
        ],
    );
    let requests = Arc::new(Mutex::new(Vec::new()));

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .with_tool_authorizer(RecordingAuthorizer::allow(requests.clone()))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "read the readme".to_string(),
        }])
        .await
        .expect("send");

    let requests = requests.lock().expect("requests poisoned");
    assert_eq!(requests.len(), 1);
    let operations = requests[0].preview.structured_input["operations"]
        .as_array()
        .expect("operations array");
    assert_eq!(operations[0]["op"].as_str(), Some("read"));
    assert!(
        operations[0]["resolved_path"]
            .as_str()
            .expect("resolved path")
            .ends_with("README.md")
    );
    assert!(operations[0].get("content").is_none());
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
        RuntimeHookEvent::ModelResponseFinished {
            success: true,
            stop_reason: Some(reason),
            usage: None,
            ..
        } if reason == "tool_use"
    )));
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
            if content.to_display_string() == "configured"
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

#[tokio::test]
async fn model_response_finished_hook_reports_usage_after_successful_commit() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![ok_stream(vec![
            ProviderEvent::MessageStarted {
                id: "msg-usage".to_string(),
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
            ProviderEvent::MessageDelta {
                stop_reason: Some("end_turn".to_string()),
                usage: Some(TokenUsage {
                    input_tokens: Some(12),
                    output_tokens: Some(5),
                    total_tokens: Some(17),
                    ..TokenUsage::default()
                }),
            },
            ProviderEvent::MessageStopped,
        ])],
    );
    let recorded = Arc::new(Mutex::new(Vec::new()));

    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
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
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeHookEvent::ModelResponseFinished {
            success: true,
            stop_reason: Some(reason),
            usage: Some(TokenUsage {
                input_tokens: Some(12),
                output_tokens: Some(5),
                total_tokens: Some(17),
                ..
            }),
            ..
        } if reason == "end_turn"
    )));
}

#[tokio::test]
async fn model_response_finished_hook_reports_stream_failures_without_usage() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![erroring_stream(
            vec![
                ProviderEvent::MessageStarted {
                    id: "msg-fail".to_string(),
                    model: model.id.clone(),
                    role: Role::Assistant,
                },
                ProviderEvent::ContentBlockStarted {
                    index: 0,
                    kind: ContentBlockStart::Text,
                },
                ProviderEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::Text("par".to_string()),
                },
            ],
            ProviderError::MalformedStream("boom".to_string()),
        )],
    );
    let recorded = Arc::new(Mutex::new(Vec::new()));

    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_hook(RecordingHook {
            events: recorded.clone(),
        })
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).unwrap();

    let error = agent
        .send(vec![ContentBlock::Text {
            text: "hi".to_string(),
        }])
        .await
        .expect_err("send should fail");
    assert!(matches!(
        error,
        crate::runtime::RuntimeError::FailedToStreamResponse(ProviderError::MalformedStream(_))
    ));

    let events = recorded.lock().expect("hook events poisoned").clone();
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeHookEvent::ModelResponseFinished {
            success: false,
            usage: None,
            error: Some(message),
            ..
        } if message.contains("malformed provider stream: boom")
    )));
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
impl ToolDefinition for AppContextTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("app_context_tool")
            .description("Return a value from the runtime app context.")
            .input_schema(json!({
                "type": "object",
                "properties": {}
            }))
            .build()
    }
}

#[async_trait]
impl ToolExecutor for AppContextTool {
    async fn execute_mut(&self, ctx: ToolContext<'_>, _input: Value) -> ToolResult {
        Ok(ctx.app_context::<TestAppState>()?.label.to_string())
    }
}

struct SlowTool;

#[async_trait]
impl ToolDefinition for SlowTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("slow_tool")
            .description("Sleep long enough to trigger a timeout.")
            .input_schema(json!({
                "type": "object",
                "properties": {}
            }))
            .execution_timeout(Duration::from_millis(20))
            .build()
    }
}

#[async_trait]
impl ToolExecutor for SlowTool {
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

enum AuthorizerBehavior {
    Allow,
    Prompt(String),
    Deny(String),
    Error(String),
    DelayedAllow(Duration),
}

struct RecordingAuthorizer {
    behavior: AuthorizerBehavior,
    requests: Arc<Mutex<Vec<ToolAuthorizationRequest>>>,
    timeout: Option<Duration>,
}

impl RecordingAuthorizer {
    fn allow(requests: Arc<Mutex<Vec<ToolAuthorizationRequest>>>) -> Self {
        Self {
            behavior: AuthorizerBehavior::Allow,
            requests,
            timeout: None,
        }
    }

    fn prompt(reason: &str, requests: Arc<Mutex<Vec<ToolAuthorizationRequest>>>) -> Self {
        Self {
            behavior: AuthorizerBehavior::Prompt(reason.to_string()),
            requests,
            timeout: None,
        }
    }

    fn error(reason: &str) -> Self {
        Self {
            behavior: AuthorizerBehavior::Error(reason.to_string()),
            requests: Arc::new(Mutex::new(Vec::new())),
            timeout: None,
        }
    }

    fn deny(reason: &str, requests: Arc<Mutex<Vec<ToolAuthorizationRequest>>>) -> Self {
        Self {
            behavior: AuthorizerBehavior::Deny(reason.to_string()),
            requests,
            timeout: None,
        }
    }

    fn delayed_allow(delay: Duration, timeout: Duration) -> Self {
        Self {
            behavior: AuthorizerBehavior::DelayedAllow(delay),
            requests: Arc::new(Mutex::new(Vec::new())),
            timeout: Some(timeout),
        }
    }
}

#[async_trait]
impl ToolAuthorizer for RecordingAuthorizer {
    async fn authorize(
        &self,
        request: &ToolAuthorizationRequest,
    ) -> Result<ToolAuthorizationDecision, crate::runtime::RuntimeError> {
        self.requests
            .lock()
            .expect("requests poisoned")
            .push(request.clone());

        match &self.behavior {
            AuthorizerBehavior::Allow => Ok(ToolAuthorizationDecision::allow()),
            AuthorizerBehavior::Prompt(reason) => {
                Ok(ToolAuthorizationDecision::prompt(reason.clone()))
            }
            AuthorizerBehavior::Deny(reason) => Ok(ToolAuthorizationDecision::deny(reason.clone())),
            AuthorizerBehavior::Error(reason) => {
                Err(crate::runtime::RuntimeError::Store(reason.clone()))
            }
            AuthorizerBehavior::DelayedAllow(delay) => {
                sleep(*delay).await;
                Ok(ToolAuthorizationDecision::allow())
            }
        }
    }

    fn timeout(&self) -> Option<Duration> {
        self.timeout
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
        ProviderEvent::MessageDelta {
            stop_reason: Some("end_turn".to_string()),
            usage: None,
        },
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
        ProviderEvent::MessageDelta {
            stop_reason: Some("tool_use".to_string()),
            usage: None,
        },
        ProviderEvent::MessageStopped,
    ])
}
