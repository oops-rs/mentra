use std::{
    collections::VecDeque,
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use mentra::{
    Agent, BuiltinProvider, ContentBlock, FileToolProfile, ModelInfo, ModelSelector, Runtime,
    error::RuntimeError,
    provider::{
        Provider, ProviderDescriptor, ProviderError, ProviderEventStream, ProviderId, Request,
        Response, Role, provider_event_stream_from_response,
    },
    runtime::{
        CommandOutput, CommandRequest, RuntimeExecutor, RuntimePolicy, VolatileRuntimeStore,
    },
    tool::{ParallelToolContext, ToolContext, ToolDefinition, ToolExecutor, ToolResult, ToolSpec},
};
use serde_json::{Value, json};

#[derive(Debug)]
enum Turn {
    Text(String),
    ToolCalls(Vec<ScriptedToolCall>),
}

#[derive(Debug, Clone)]
struct ScriptedToolCall {
    id: Option<String>,
    name: String,
    input: Value,
}

impl ScriptedToolCall {
    fn new(name: impl Into<String>, input: Value) -> Self {
        Self {
            id: None,
            name: name.into(),
            input,
        }
    }
}

#[derive(Clone)]
struct ScriptedProvider {
    kind: ProviderId,
    models: Vec<ModelInfo>,
    turns: Arc<Mutex<VecDeque<Turn>>>,
    requests: Arc<Mutex<Vec<Request<'static>>>>,
}

impl ScriptedProvider {
    fn new(kind: ProviderId, models: Vec<ModelInfo>) -> Self {
        Self {
            kind,
            models,
            turns: Arc::new(Mutex::new(VecDeque::new())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn push_turns(&self, turns: Vec<Turn>) {
        let mut queue = self.turns.lock().expect("scripted turn queue poisoned");
        queue.extend(turns);
    }

    fn recorded_requests(&self) -> Vec<Request<'static>> {
        self.requests
            .lock()
            .expect("scripted request log poisoned")
            .clone()
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(self.kind.clone())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(self.models.clone())
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        self.requests
            .lock()
            .expect("scripted request log poisoned")
            .push(request.into_owned());

        let turn = self
            .turns
            .lock()
            .expect("scripted turn queue poisoned")
            .pop_front()
            .unwrap_or_else(|| panic!("no scripted turn remaining for public API test"));

        match turn {
            Turn::Text(text) => Ok(provider_event_stream_from_response(Response {
                id: format!("public-response-{}", now_nanos()),
                model: self.models[0].id.clone(),
                role: Role::Assistant,
                content: vec![ContentBlock::text(text)],
                stop_reason: None,
                usage: None,
            })),
            Turn::ToolCalls(calls) => Ok(provider_event_stream_from_response(Response {
                id: format!("public-response-{}", now_nanos()),
                model: self.models[0].id.clone(),
                role: Role::Assistant,
                content: calls
                    .into_iter()
                    .enumerate()
                    .map(|(index, call)| ContentBlock::ToolUse {
                        id: call.id.unwrap_or_else(|| format!("tool-{}", index + 1)),
                        name: call.name,
                        input: call.input,
                    })
                    .collect(),
                stop_reason: Some("tool_use".to_string()),
                usage: None,
            })),
        }
    }
}

struct Harness {
    runtime: Runtime,
    provider: ScriptedProvider,
    model: ModelInfo,
}

impl Harness {
    fn new(turns: Vec<Turn>) -> Self {
        let runtime_id = format!("public-api-{}", now_nanos());
        let model = ModelInfo::new("mock-model", BuiltinProvider::OpenAI);
        let provider = ScriptedProvider::new(model.provider.clone(), vec![model.clone()]);
        provider.push_turns(turns);

        let runtime = Runtime::builder()
            .with_runtime_identifier(runtime_id)
            .with_store(VolatileRuntimeStore::new())
            .with_provider_instance(provider.clone())
            .build()
            .expect("build runtime");

        Self {
            runtime,
            provider,
            model,
        }
    }

    fn spawn(&self, name: &str) -> Agent {
        self.runtime
            .spawn(name, self.model.clone())
            .expect("spawn test agent")
    }

    async fn recorded_requests(&self) -> Vec<Request<'static>> {
        self.provider.recorded_requests()
    }
}

struct EchoTool;

struct AlphaTool;

struct EndTurnTool;

struct SubagentSummaryTool;

#[async_trait]
impl ToolDefinition for EchoTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("echo_tool")
            .description("Echo a canned result")
            .input_schema(json!({
                "type": "object",
                "properties": {}
            }))
            .build()
    }
}

#[async_trait]
impl ToolExecutor for EchoTool {
    async fn execute(&self, _ctx: ParallelToolContext, _input: Value) -> ToolResult {
        Ok("echoed".to_string())
    }
}

#[async_trait]
impl ToolDefinition for AlphaTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("alpha_tool")
            .description("Return a canned alpha result")
            .input_schema(json!({
                "type": "object",
                "properties": {}
            }))
            .build()
    }
}

#[async_trait]
impl ToolExecutor for AlphaTool {
    async fn execute(&self, _ctx: ParallelToolContext, _input: Value) -> ToolResult {
        Ok("alpha".to_string())
    }
}

#[async_trait]
impl ToolDefinition for EndTurnTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("stop_here")
            .description("End the current turn without a follow-up assistant message")
            .input_schema(json!({
                "type": "object",
                "properties": {}
            }))
            .build()
    }
}

#[async_trait]
impl ToolExecutor for EndTurnTool {
    async fn execute_mut(&self, mut ctx: ToolContext<'_>, _input: Value) -> ToolResult {
        ctx.request_idle();
        Ok("stopping now".to_string())
    }
}

#[async_trait]
impl ToolDefinition for SubagentSummaryTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("subagent_summary")
            .description("Spawn a disposable subagent and return its summary")
            .input_schema(json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string" }
                },
                "required": ["prompt"]
            }))
            .build()
    }
}

#[async_trait]
impl ToolExecutor for SubagentSummaryTool {
    async fn execute(&self, ctx: ParallelToolContext, input: Value) -> ToolResult {
        let prompt = input
            .get("prompt")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "prompt is required".to_string())?;
        let mut child = ctx.spawn_subagent().map_err(|error| error.to_string())?;
        // The public pattern for a custom tool that spawns work: the child runs
        // under the parent run's derived bounds, not a fresh unbounded set.
        let message = child
            .run(vec![ContentBlock::text(prompt)], ctx.child_run_options())
            .await
            .map_err(|error| format!("child failed: {error}"))?;
        Ok(message.text())
    }
}

#[tokio::test]
async fn send_returns_final_message_after_tool_execution() {
    let harness = Harness::new(vec![
        Turn::ToolCalls(vec![ScriptedToolCall::new("echo_tool", json!({}))]),
        Turn::Text("done".to_string()),
    ]);
    harness.runtime.register_tool(EchoTool);
    let mut agent = harness.spawn("tool-agent");

    let message = agent
        .send(vec![ContentBlock::text("run the tool")])
        .await
        .unwrap();

    assert_eq!(message.role, Role::Assistant);
    assert_eq!(message.text(), "done");
    assert_eq!(harness.recorded_requests().await.len(), 2);
}

#[tokio::test]
async fn runtime_exposes_registered_tool_descriptors() {
    let runtime_id = format!("public-api-{}", now_nanos());
    let model = ModelInfo::new("mock-model", BuiltinProvider::OpenAI);
    let provider = ScriptedProvider::new(model.provider.clone(), vec![model.clone()]);

    let runtime = Runtime::empty_builder()
        .with_runtime_identifier(runtime_id)
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    runtime.register_tool(EchoTool);
    runtime.register_tool(AlphaTool);

    assert_eq!(
        runtime.tools(),
        vec![AlphaTool.descriptor(), EchoTool.descriptor()]
    );
    assert_eq!(
        runtime.tool_descriptor("echo_tool"),
        Some(EchoTool.descriptor())
    );
    assert_eq!(
        runtime.tool_descriptor("alpha_tool"),
        Some(AlphaTool.descriptor())
    );
    assert_eq!(runtime.tool_descriptor("missing_tool"), None);
}

#[test]
fn runtime_builder_publicly_selects_split_file_tools() {
    let model = ModelInfo::new("mock-model", BuiltinProvider::OpenAI);
    let provider = ScriptedProvider::new(model.provider.clone(), vec![model]);
    let runtime = Runtime::builder()
        .with_file_tools(FileToolProfile::Split)
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let names = runtime
        .tools()
        .into_iter()
        .map(|tool| tool.provider.name)
        .collect::<std::collections::BTreeSet<_>>();

    for name in ["read", "ls", "grep", "glob", "write", "edit"] {
        assert!(names.contains(name), "missing split tool {name}");
    }
    assert!(!names.contains("files"));
}

#[tokio::test]
async fn parallel_tool_context_can_spawn_subagents_from_public_api() {
    let harness = Harness::new(vec![
        Turn::ToolCalls(vec![ScriptedToolCall::new(
            "subagent_summary",
            json!({ "prompt": "summarize the delegated work" }),
        )]),
        Turn::Text("child summary".to_string()),
        Turn::Text("parent complete".to_string()),
    ]);
    harness.runtime.register_tool(SubagentSummaryTool);
    let mut agent = harness.spawn("parent-agent");

    let message = agent
        .send(vec![ContentBlock::text("delegate that")])
        .await
        .unwrap();

    assert_eq!(message.role, Role::Assistant);
    assert_eq!(message.text(), "parent complete");
    assert_eq!(harness.recorded_requests().await.len(), 3);
}

#[tokio::test]
async fn empty_assistant_response_preserves_committed_tool_results() {
    let harness = Harness::new(vec![Turn::ToolCalls(vec![ScriptedToolCall::new(
        "stop_here",
        json!({}),
    )])]);
    harness.runtime.register_tool(EndTurnTool);
    let mut agent = harness.spawn("idle-agent");

    let error = agent
        .send(vec![ContentBlock::text("stop after the tool")])
        .await
        .unwrap_err();

    assert!(matches!(error, RuntimeError::EmptyAssistantResponse));
    assert_eq!(harness.recorded_requests().await.len(), 1);
    assert_eq!(agent.history().len(), 3);
    match &agent.history()[2].content[0] {
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            assert_eq!(tool_use_id, "tool-1");
            assert_eq!(content, "stopping now");
            assert!(!is_error);
        }
        other => panic!("expected tool result block, found {other:?}"),
    }
}

#[tokio::test]
async fn resolve_model_returns_explicit_id_without_listing_models() {
    let runtime_id = format!("public-api-{}", now_nanos());
    let provider = FailingListModelsProvider {
        kind: BuiltinProvider::Anthropic.into(),
    };

    let runtime = Runtime::builder()
        .with_runtime_identifier(runtime_id)
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");

    let model = runtime
        .resolve_model(
            BuiltinProvider::Anthropic,
            ModelSelector::Id("claude-custom".to_string()),
        )
        .await
        .expect("resolve explicit model");

    assert_eq!(
        model,
        ModelInfo::new("claude-custom", BuiltinProvider::Anthropic)
    );
}

#[tokio::test]
async fn resolve_model_selects_newest_available_then_breaks_ties_by_id() {
    let runtime_id = format!("public-api-{}", now_nanos());
    let provider = ModelListingProvider {
        kind: BuiltinProvider::OpenAI.into(),
        models: vec![
            model_with_created_at("zeta", BuiltinProvider::OpenAI, 1_700_000_100),
            model_with_created_at("alpha", BuiltinProvider::OpenAI, 1_700_000_100),
            model_with_created_at("older", BuiltinProvider::OpenAI, 1_700_000_000),
        ],
    };

    let runtime = Runtime::builder()
        .with_runtime_identifier(runtime_id)
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");

    let model = runtime
        .resolve_model(BuiltinProvider::OpenAI, ModelSelector::NewestAvailable)
        .await
        .expect("resolve newest model");

    assert_eq!(
        model,
        model_with_created_at("alpha", BuiltinProvider::OpenAI, 1_700_000_100)
    );
}

#[tokio::test]
async fn resolve_model_reports_empty_provider_listing() {
    let runtime_id = format!("public-api-{}", now_nanos());
    let provider = ModelListingProvider {
        kind: BuiltinProvider::Gemini.into(),
        models: Vec::new(),
    };

    let runtime = Runtime::builder()
        .with_runtime_identifier(runtime_id)
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");

    let error = runtime
        .resolve_model(BuiltinProvider::Gemini, ModelSelector::NewestAvailable)
        .await
        .expect_err("empty listing should fail");

    assert!(matches!(
        error,
        RuntimeError::NoModelsAvailable(provider) if provider == BuiltinProvider::Gemini.into()
    ));
}

#[tokio::test]
async fn resolve_model_supports_openrouter_provider() {
    let runtime_id = format!("public-api-{}", now_nanos());
    let provider = ModelListingProvider {
        kind: BuiltinProvider::OpenRouter.into(),
        models: vec![model_with_created_at(
            "openai/gpt-4.1-mini",
            BuiltinProvider::OpenRouter,
            1_741_049_700,
        )],
    };

    let runtime = Runtime::builder()
        .with_runtime_identifier(runtime_id)
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");

    let model = runtime
        .resolve_model(BuiltinProvider::OpenRouter, ModelSelector::NewestAvailable)
        .await
        .expect("resolve newest model");

    assert_eq!(
        model,
        model_with_created_at(
            "openai/gpt-4.1-mini",
            BuiltinProvider::OpenRouter,
            1_741_049_700,
        )
    );
}

#[tokio::test]
async fn resolve_model_supports_ollama_provider_registration() {
    let runtime = Runtime::empty_builder()
        .with_ollama()
        .build()
        .expect("build runtime");

    let model = runtime
        .resolve_model(
            BuiltinProvider::Ollama,
            ModelSelector::Id("qwen2.5-coder".to_string()),
        )
        .await
        .expect("resolve explicit model");

    assert_eq!(
        model,
        ModelInfo::new("qwen2.5-coder", BuiltinProvider::Ollama)
    );
}

#[tokio::test]
async fn resolve_model_supports_lmstudio_provider_registration() {
    let runtime = Runtime::empty_builder()
        .with_lmstudio()
        .build()
        .expect("build runtime");

    let model = runtime
        .resolve_model(
            BuiltinProvider::LmStudio,
            ModelSelector::Id("local-model".to_string()),
        )
        .await
        .expect("resolve explicit model");

    assert_eq!(
        model,
        ModelInfo::new("local-model", BuiltinProvider::LmStudio)
    );
}

#[tokio::test]
async fn resolve_model_reports_missing_provider() {
    let harness = Harness::new(vec![Turn::Text("unused".to_string())]);

    let error = harness
        .runtime
        .resolve_model(
            BuiltinProvider::Gemini,
            ModelSelector::Id("gemini-2.5-pro".to_string()),
        )
        .await
        .expect_err("missing provider should fail");

    assert!(matches!(
        error,
        RuntimeError::ProviderNotFound(Some(provider))
            if provider == BuiltinProvider::Gemini.into()
    ));
}

#[tokio::test]
async fn runtime_accepts_provider_core_openai_compatible_instances() {
    let provider_id = ProviderId::new("custom-openai-compatible");
    let (base_url, handle) = spawn_models_server(
        r#"{"data":[{"id":"compat-model","name":"Compat Model","created":1}]}"#,
    );

    let mut definition = mentra::provider_core::responses::openai_definition();
    definition.descriptor.id = provider_id.clone();
    definition.descriptor.display_name = Some("Custom OpenAI-Compatible".to_string());
    definition.base_url = Some(base_url);

    let runtime = Runtime::empty_builder()
        .with_registered_provider(mentra::provider_core::responses::ResponsesProvider::new(
            definition,
            mentra::provider_core::StaticCredentialSource::new("test-key"),
        ))
        .build()
        .expect("build runtime");

    let model = runtime
        .resolve_model(provider_id.clone(), ModelSelector::NewestAvailable)
        .await
        .expect("resolve model from provider-core instance");

    assert_eq!(model.provider, provider_id);
    assert_eq!(model.id, "compat-model");

    let captured = handle.join().expect("capture request");
    let captured_lower = captured.to_ascii_lowercase();
    assert!(captured.starts_with("GET /v1/models HTTP/1.1\r\n"));
    assert!(captured_lower.contains("authorization: bearer test-key\r\n"));
}

#[derive(Clone)]
struct FailingListModelsProvider {
    kind: ProviderId,
}

#[async_trait]
impl Provider for FailingListModelsProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(self.kind.clone())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Err(ProviderError::InvalidResponse(
            "list_models should not be called".to_string(),
        ))
    }

    async fn stream(&self, _request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Ok(rx)
    }
}

#[derive(Clone)]
struct ModelListingProvider {
    kind: ProviderId,
    models: Vec<ModelInfo>,
}

#[async_trait]
impl Provider for ModelListingProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(self.kind.clone())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(self.models.clone())
    }

    async fn stream(&self, _request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Ok(rx)
    }
}

/// The builder was `pub` inside a private `mod builder`, re-exported nowhere:
/// `Runtime::builder()` worked on inference, but a downstream helper taking or
/// returning a half-built runtime could not write its signature at all. This
/// test is that helper, written from outside the crate — it pins the re-export
/// at both public paths, because compiling is the whole claim.
#[test]
fn a_runtime_builder_is_a_type_downstream_code_can_name() {
    fn with_volatile_store(builder: mentra::RuntimeBuilder) -> mentra::runtime::RuntimeBuilder {
        builder.with_store(VolatileRuntimeStore::new())
    }

    let _ = with_volatile_store(Runtime::builder());
}

/// The two paths a downstream crate re-exports these from, written from
/// outside mentra so a rename is a failing test rather than a broken host.
///
/// basis re-exports `ProviderRetry` and `ResponsesTransport` from its own
/// `basis::runtime` so a host of *its* never names mentra, which makes both
/// paths part of this crate's contract rather than an implementation detail
/// that happens to be reachable. `ResponsesTransport` is visible at the crate
/// root too; `mentra::provider::` is the one to depend on, since it sits with
/// `ResponsesRequestOptions` and the rest of the wire vocabulary.
#[test]
fn the_retry_schedule_and_transport_are_types_downstream_code_can_name() {
    fn patient(retry: mentra::runtime::ProviderRetry) -> mentra::runtime::RunOptions {
        mentra::runtime::RunOptions::default().with_provider_retry(retry)
    }

    fn over(transport: mentra::provider::ResponsesTransport) -> mentra::RuntimeBuilder {
        Runtime::empty_builder().with_responses_transport(transport)
    }

    let options = patient(mentra::runtime::ProviderRetry {
        base_delay: std::time::Duration::from_secs(1),
        max_delay: std::time::Duration::from_secs(30),
        ..Default::default()
    });
    let _ = over(mentra::provider::ResponsesTransport::HttpSse);

    // The delegation contract basis depends on, asserted from outside: a
    // subagent meets the same provider with the same patience, so a host
    // states its schedule once rather than at every boundary.
    let child = options.child();
    assert_eq!(child.provider_retry, options.provider_retry);
    assert_eq!(child.retry_budget, options.retry_budget);
}

/// The downstream shape this exists for, written from outside the crate: a
/// host registers an executor that serves named targets and a tool that names
/// one. Every guard around a shell command still applies — only the executor
/// reads the name. Compiling is half the claim; the other half is that the
/// name survives the trip.
#[tokio::test]
async fn a_tool_can_name_the_executor_a_command_runs_on() {
    #[derive(Clone, Default)]
    struct TargetLog(Arc<Mutex<Vec<Option<String>>>>);

    #[async_trait]
    impl RuntimeExecutor for TargetLog {
        async fn run(&self, request: CommandRequest) -> Result<CommandOutput, String> {
            self.0
                .lock()
                .expect("target log poisoned")
                .push(request.target.clone());
            Ok(CommandOutput {
                stdout: format!("ran on {}", request.target.unwrap_or("local".to_string())),
                stderr: String::new(),
                success: true,
                status_code: Some(0),
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
            })
        }
    }

    struct TargetedShellTool;

    #[async_trait]
    impl ToolDefinition for TargetedShellTool {
        fn descriptor(&self) -> ToolSpec {
            ToolSpec::builder("targeted_shell")
                .description("Run a command on a named host")
                .input_schema(json!({
                    "type": "object",
                    "properties": {
                        "target": { "type": "string" },
                        "command": { "type": "string" }
                    },
                    "required": ["command"]
                }))
                .build()
        }
    }

    #[async_trait]
    impl ToolExecutor for TargetedShellTool {
        async fn execute(&self, ctx: ParallelToolContext, input: Value) -> ToolResult {
            let command = input
                .get("command")
                .and_then(Value::as_str)
                .ok_or_else(|| "command is required".to_string())?
                .to_string();
            let target = input
                .get("target")
                .and_then(Value::as_str)
                .map(str::to_string);
            let cwd = ctx.resolve_working_directory(None)?;
            let output = ctx
                .execute_shell_command_on(target, command, None, None, cwd)
                .await?;
            Ok(output.stdout)
        }
    }

    let log = TargetLog::default();
    let model = ModelInfo::new("mock-model", BuiltinProvider::OpenAI);
    let provider = ScriptedProvider::new(model.provider.clone(), vec![model.clone()]);
    provider.push_turns(vec![
        Turn::ToolCalls(vec![ScriptedToolCall::new(
            "targeted_shell",
            json!({ "target": "mac", "command": "xcodebuild -version" }),
        )]),
        Turn::Text("done".to_string()),
    ]);

    let runtime = Runtime::builder()
        .with_runtime_identifier(format!("public-api-{}", now_nanos()))
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .with_policy(RuntimePolicy::permissive())
        .with_executor(log.clone())
        .build()
        .expect("build runtime");
    runtime.register_tool(TargetedShellTool);
    let mut agent = runtime.spawn("target-agent", model).expect("spawn agent");

    agent
        .send(vec![ContentBlock::text("build it on the mac")])
        .await
        .expect("run completes");

    assert_eq!(
        log.0.lock().expect("target log poisoned").as_slice(),
        [Some("mac".to_string())]
    );
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn spawn_models_server(response_body: &str) -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let addr = listener.local_addr().expect("read listener addr");
    let response_body = response_body.to_string();

    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept request");
        let mut request = Vec::new();
        let mut temp = [0_u8; 1024];

        loop {
            let read = stream.read(&mut temp).expect("read request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&temp[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }

        let response = format!(
            concat!(
                "HTTP/1.1 200 OK\r\n",
                "content-type: application/json\r\n",
                "content-length: {}\r\n\r\n",
                "{}"
            ),
            response_body.len(),
            response_body
        );
        stream
            .write_all(response.as_bytes())
            .expect("write response");

        String::from_utf8(request).expect("request should be valid utf8")
    });

    (format!("http://{addr}/"), handle)
}

fn model_with_created_at(id: &str, provider: BuiltinProvider, unix_timestamp: i64) -> ModelInfo {
    let mut model = ModelInfo::new(id, provider);
    model.created_at = Some(
        time::OffsetDateTime::from_unix_timestamp(unix_timestamp)
            .expect("timestamp should be valid"),
    );
    model
}

#[test]
fn an_openai_compatible_endpoint_can_be_registered_before_the_runtime_exists() {
    // A host that must settle its provider before it has a runtime could not
    // use the post-build registration, and the keyless case had no way to say
    // "no credentials" through a static credential source.
    let runtime = Runtime::empty_builder()
        .with_openai_compatible(
            "deepseek",
            "https://api.deepseek.com/",
            Some("key".to_string()),
        )
        .with_openai_compatible("local-vllm", "http://127.0.0.1:8000/", None)
        .build()
        .expect("runtime builds");

    let registered: Vec<String> = runtime
        .providers()
        .into_iter()
        .map(|provider| provider.id.as_str().to_string())
        .collect();

    assert!(
        registered.contains(&"deepseek".to_string()),
        "{registered:?}"
    );
    assert!(
        registered.contains(&"local-vllm".to_string()),
        "{registered:?}"
    );
}
