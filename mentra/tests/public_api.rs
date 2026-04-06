use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use mentra::{
    Agent, BuiltinProvider, ContentBlock, ModelInfo, ModelSelector, Runtime,
    error::RuntimeError,
    provider::{
        Provider, ProviderDescriptor, ProviderError, ProviderEventStream, ProviderId, Request,
        Response, Role, provider_event_stream_from_response,
    },
    runtime::SqliteRuntimeStore,
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
        let store_path = std::env::temp_dir().join(format!("{runtime_id}.sqlite"));
        let model = ModelInfo::new("mock-model", BuiltinProvider::OpenAI);
        let provider = ScriptedProvider::new(model.provider.clone(), vec![model.clone()]);
        provider.push_turns(turns);

        let runtime = Runtime::builder()
            .with_runtime_identifier(runtime_id)
            .with_store(SqliteRuntimeStore::new(store_path))
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
        let message = child
            .send(vec![ContentBlock::text(prompt)])
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
    let store_path = std::env::temp_dir().join(format!("{runtime_id}.sqlite"));
    let model = ModelInfo::new("mock-model", BuiltinProvider::OpenAI);
    let provider = ScriptedProvider::new(model.provider.clone(), vec![model.clone()]);

    let runtime = Runtime::empty_builder()
        .with_runtime_identifier(runtime_id)
        .with_store(SqliteRuntimeStore::new(store_path))
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
    let store_path = std::env::temp_dir().join(format!("{runtime_id}.sqlite"));
    let provider = FailingListModelsProvider {
        kind: BuiltinProvider::Anthropic.into(),
    };

    let runtime = Runtime::builder()
        .with_runtime_identifier(runtime_id)
        .with_store(SqliteRuntimeStore::new(store_path))
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
    let store_path = std::env::temp_dir().join(format!("{runtime_id}.sqlite"));
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
        .with_store(SqliteRuntimeStore::new(store_path))
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
    let store_path = std::env::temp_dir().join(format!("{runtime_id}.sqlite"));
    let provider = ModelListingProvider {
        kind: BuiltinProvider::Gemini.into(),
        models: Vec::new(),
    };

    let runtime = Runtime::builder()
        .with_runtime_identifier(runtime_id)
        .with_store(SqliteRuntimeStore::new(store_path))
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
    let store_path = std::env::temp_dir().join(format!("{runtime_id}.sqlite"));
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
        .with_store(SqliteRuntimeStore::new(store_path))
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

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn model_with_created_at(id: &str, provider: BuiltinProvider, unix_timestamp: i64) -> ModelInfo {
    let mut model = ModelInfo::new(id, provider);
    model.created_at = Some(
        time::OffsetDateTime::from_unix_timestamp(unix_timestamp)
            .expect("timestamp should be valid"),
    );
    model
}
