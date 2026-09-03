use std::{
    collections::{HashSet, VecDeque},
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use mentra::{
    Agent, BuiltinProvider, ContentBlock, FileToolProfile, ModelInfo, ModelSelector, Runtime,
    ToolAudience,
    agent::{AgentEvent, AgentEventTapGuard},
    error::RuntimeError,
    provider::{
        Provider, ProviderDescriptor, ProviderError, ProviderEventStream, ProviderId, Request,
        Response, Role, provider_event_stream_from_response,
    },
    runtime::{
        BeforeDecision, CommandOutput, CommandRequest, ExecutionHookParticipant,
        ExecutionHookRegistration, HookDecision, PostExecutionContext, PostExecutionHook,
        PostExecutionHookRegistration, PreExecutionContext, PreExecutionHook,
        PreExecutionHookRegistration, ResultDecision, RuntimeExecutor, RuntimePolicy,
        SessionOptions, VolatileRuntimeStore,
    },
    tool::{
        ExecutableTool, ParallelToolContext, ToolAuthorizationDecision, ToolAuthorizationPreview,
        ToolAuthorizationRequest, ToolAuthorizer, ToolContext, ToolDefinition, ToolExecutor,
        ToolResult, ToolSideEffectLevel, ToolSpec,
    },
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

struct PublicExecutionHook;

struct PublicMixedHook(&'static str);

#[async_trait]
impl ExecutionHookParticipant for PublicMixedHook {
    fn name(&self) -> &str {
        self.0
    }

    async fn before(&self, _context: &PreExecutionContext) -> Result<BeforeDecision, RuntimeError> {
        Ok(BeforeDecision::Continue)
    }
}

#[async_trait]
impl PreExecutionHook for PublicExecutionHook {
    async fn pre_tool_execution(
        &self,
        _context: &PreExecutionContext,
    ) -> Result<HookDecision, RuntimeError> {
        Ok(HookDecision::Allow)
    }
}

#[async_trait]
impl PostExecutionHook for PublicExecutionHook {
    async fn post_tool_execution(
        &self,
        _context: &PostExecutionContext,
    ) -> Result<ResultDecision, RuntimeError> {
        Ok(ResultDecision::Keep)
    }
}

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
async fn a_session_exposes_the_lossless_agent_event_tap_to_embedders() {
    let harness = Harness::new(vec![
        Turn::ToolCalls(vec![ScriptedToolCall::new("echo_tool", json!({}))]),
        Turn::Text("done".to_string()),
    ]);
    harness.runtime.register_tool(EchoTool);
    let mut session = harness
        .runtime
        .create_session("observed-session", harness.model.clone())
        .expect("create observed session");
    let observed = Arc::new(Mutex::new(Vec::<AgentEvent>::new()));
    let observed_for_tap = Arc::clone(&observed);

    // Spelling the guard type here proves both sides of the embedding API are
    // nameable from a downstream crate rather than only from mentra itself.
    let tap: AgentEventTapGuard = session.register_agent_event_tap(move |event| {
        observed_for_tap
            .lock()
            .expect("agent event observation log poisoned")
            .push(event.clone());
    });

    let message = session
        .append_turn(vec![ContentBlock::text("run the tool")])
        .await
        .expect("observed session completes");
    assert_eq!(message.text(), "done");

    let events = observed
        .lock()
        .expect("agent event observation log poisoned");
    assert!(matches!(events.first(), Some(AgentEvent::RunStarted)));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolExecutionFinished {
            result: ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error: false,
            },
        } if tool_use_id == "tool-1" && content.to_display_string() == "echoed"
    )));
    assert!(matches!(events.last(), Some(AgentEvent::RunFinished)));
    drop(events);
    drop(tap);
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

#[test]
fn runtime_builder_publicly_disables_builtin_file_tools() {
    let model = ModelInfo::new("mock-model", BuiltinProvider::OpenAI);
    let provider = ScriptedProvider::new(model.provider.clone(), vec![model]);
    let runtime = Runtime::builder()
        .with_file_tools(FileToolProfile::None)
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");

    for name in ["files", "read", "ls", "grep", "glob", "write", "edit"] {
        assert!(
            runtime.tool_descriptor(name).is_none(),
            "file tool descriptor remained: {name}"
        );
    }
    assert!(runtime.tool_descriptor("shell").is_some());
}

#[test]
fn runtime_publicly_registers_audience_tools_with_guard_lifetimes() {
    let model = ModelInfo::new("mock-model", BuiltinProvider::OpenAI);
    let provider = ScriptedProvider::new(model.provider.clone(), vec![model.clone()]);
    let runtime = Runtime::empty_builder()
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let audience = ToolAudience::new("workspace-a");
    assert_eq!(audience.as_ref(), "workspace-a");
    assert_eq!(audience.to_string(), "workspace-a");
    assert_eq!(
        serde_json::from_str::<ToolAudience>(
            &serde_json::to_string(&audience).expect("serialize audience")
        )
        .expect("deserialize audience"),
        audience
    );
    assert_eq!(
        HashSet::from([audience.clone(), ToolAudience::from("workspace-a")]).len(),
        1
    );

    let guard = runtime
        .try_register_tool_for_audience(audience.clone(), EchoTool)
        .expect("register audience tool");
    assert_eq!(guard.audience(), &audience);
    assert_eq!(guard.descriptor(), &EchoTool.descriptor());
    assert!(runtime.tool_descriptor("echo_tool").is_none());
    let agent = runtime
        .spawn_with_config_for_audience(
            "audience-agent",
            model.clone(),
            Default::default(),
            audience.clone(),
        )
        .expect("spawn audience agent");
    assert_eq!(agent.tool_audience(), Some(&audience));
    let session = runtime
        .create_session_with_options(
            "audience-session",
            model,
            SessionOptions {
                config: Default::default(),
                policy: None,
                tool_audience: Some(audience.clone()),
                project_id: None,
                runtime_identifier: None,
            },
        )
        .expect("create audience session");
    assert_eq!(session.tool_audience(), Some(&audience));
    assert!(
        runtime
            .try_register_tool_for_audience(audience.clone(), EchoTool)
            .is_err()
    );
    let other_guard = runtime
        .try_register_tool_for_audience(ToolAudience::from("workspace-b"), EchoTool)
        .expect("same name in another audience");
    assert!(guard.unregister());
    let replacement = runtime
        .try_register_tool_for_audience(audience, EchoTool)
        .expect("released audience name can register again");
    drop(replacement);
    drop(other_guard);
}

#[test]
fn runtime_publicly_registers_live_hook_guards_without_owning_the_runtime() {
    let model = ModelInfo::new("mock-model", BuiltinProvider::OpenAI);
    let provider = ScriptedProvider::new(model.provider.clone(), vec![model]);
    let runtime = Runtime::empty_builder()
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let audience = ToolAudience::new("hook-workspace");

    let global_pre: PreExecutionHookRegistration = runtime.register_pre_hook(PublicExecutionHook);
    let audience_pre: PreExecutionHookRegistration =
        runtime.register_pre_hook_for_audience(audience.clone(), PublicExecutionHook);
    let global_post: PostExecutionHookRegistration =
        runtime.register_post_hook(PublicExecutionHook);
    let audience_post: PostExecutionHookRegistration =
        runtime.register_post_hook_for_audience(audience.clone(), PublicExecutionHook);

    assert_eq!(global_pre.audience(), None);
    assert_eq!(audience_pre.audience(), Some(&audience));
    assert_eq!(global_post.audience(), None);
    assert_eq!(audience_post.audience(), Some(&audience));

    drop(runtime);
    assert!(!global_pre.unregister());
    assert!(!audience_pre.unregister());
    assert!(!global_post.unregister());
    assert!(!audience_post.unregister());
}

#[test]
fn runtime_publicly_registers_atomic_mixed_hook_batches() {
    let model = ModelInfo::new("mock-model", BuiltinProvider::OpenAI);
    let provider = ScriptedProvider::new(model.provider.clone(), vec![model]);
    let permanent: Arc<dyn ExecutionHookParticipant> = Arc::new(PublicMixedHook("permanent-batch"));
    let runtime = Runtime::empty_builder()
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .with_execution_hook(PublicMixedHook("permanent"))
        .with_execution_hooks([permanent])
        .build()
        .expect("build runtime");
    let audience = ToolAudience::new("mixed-workspace");

    let global_single: ExecutionHookRegistration =
        runtime.register_execution_hook(PublicMixedHook("global-single"));
    let global_batch: ExecutionHookRegistration = runtime.register_execution_hooks([
        Arc::new(PublicMixedHook("global-a")) as Arc<dyn ExecutionHookParticipant>,
        Arc::new(PublicMixedHook("global-b")) as Arc<dyn ExecutionHookParticipant>,
    ]);
    let audience_single: ExecutionHookRegistration = runtime
        .register_execution_hook_for_audience(audience.clone(), PublicMixedHook("audience-single"));
    let audience_batch: ExecutionHookRegistration = runtime.register_execution_hooks_for_audience(
        audience.clone(),
        [Arc::new(PublicMixedHook("audience-batch")) as Arc<dyn ExecutionHookParticipant>],
    );

    assert_eq!(global_single.audience(), None);
    assert_eq!(global_batch.audience(), None);
    assert_eq!(audience_single.audience(), Some(&audience));
    assert_eq!(audience_batch.audience(), Some(&audience));

    drop(runtime);
    assert!(!global_single.unregister());
    assert!(!global_batch.unregister());
    assert!(!audience_single.unregister());
    assert!(!audience_batch.unregister());
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

#[test]
fn a_session_accepts_a_scoped_tool_authorizer_after_runtime_construction() {
    struct Allow;

    #[async_trait]
    impl ToolAuthorizer for Allow {
        async fn authorize(
            &self,
            _request: &ToolAuthorizationRequest,
        ) -> Result<ToolAuthorizationDecision, RuntimeError> {
            Ok(ToolAuthorizationDecision::allow())
        }
    }

    let model = ModelInfo::new("mock-model", BuiltinProvider::OpenAI);
    let provider = ScriptedProvider::new(model.provider.clone(), vec![model.clone()]);
    let runtime = Runtime::empty_builder()
        .with_runtime_identifier(format!("public-api-{}", now_nanos()))
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");

    let _session = runtime
        .create_session("scoped-authorizer", model)
        .expect("create session")
        .with_tool_authorizer(Allow);
}

#[test]
fn permission_rule_addresses_are_public_serializable_and_exact() {
    use mentra::session::PermissionRuleScope;
    use mentra::{
        PermissionRuleAddress, PermissionRuleContext, PermissionRuleStore, RememberedRule, RuleKey,
        RuleStore,
    };

    let context = PermissionRuleContext {
        session_id: "session-1".to_owned(),
        project_id: Some("project-1".to_owned()),
    };
    assert_eq!(context.session_id, "session-1");
    assert_eq!(context.project_id.as_deref(), Some("project-1"));
    assert_eq!(
        serde_json::to_value(PermissionRuleScope::Process).expect("serialize process scope"),
        json!("process")
    );

    let address = PermissionRuleAddress {
        scope: PermissionRuleScope::Project,
        key: RuleKey {
            tool_name: "shell".to_owned(),
            pattern: Some("*cargo test*".to_owned()),
        },
    };
    assert_eq!(
        serde_json::to_value(&address).expect("serialize address"),
        json!({
            "scope": "project",
            "key": {
                "tool_name": "shell",
                "pattern": "*cargo test*",
            }
        })
    );
    let decoded: PermissionRuleAddress =
        serde_json::from_value(serde_json::to_value(&address).expect("serialize address"))
            .expect("deserialize address");
    assert_eq!(decoded, address);
    let addresses = HashSet::from([address.clone()]);
    assert!(addresses.contains(&decoded));

    let store = RuleStore::new();
    let rule = RememberedRule {
        key: address.key.clone(),
        allow: true,
        scope: address.scope,
        reason: None,
    };
    assert_eq!(
        serde_json::to_value(&rule).expect("serialize remembered rule"),
        json!({
            "key": {
                "tool_name": "shell",
                "pattern": "*cargo test*",
            },
            "allow": true,
            "scope": "project",
        }),
        "the existing RememberedRule wire shape stays unchanged"
    );
    store.add_rule(rule);
    assert!(store.revoke_rule(&address));
    assert!(!store.revoke_rule(&address));

    let persistent = mentra::runtime::VolatileRuntimeStore::new();
    persistent
        .upsert_rule(
            &context,
            &RememberedRule {
                key: address.key.clone(),
                allow: false,
                scope: address.scope,
                reason: Some("public contract".to_owned()),
            },
        )
        .expect("upsert through public store contract");
    assert_eq!(
        persistent
            .load_applicable_rules(&context)
            .expect("load through public store contract")
            .len(),
        1
    );
}

#[test]
fn sessions_expose_live_permission_rule_mutation_without_store_attachment() {
    use mentra::session::PermissionRuleScope;
    use mentra::{PermissionRuleAddress, RememberedRule, RuleKey};

    let model = ModelInfo::new("mock-model", BuiltinProvider::OpenAI);
    let provider = ScriptedProvider::new(model.provider.clone(), vec![model.clone()]);
    let runtime = Runtime::empty_builder()
        .with_runtime_identifier(format!("permission-public-{}", now_nanos()))
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let session = runtime
        .create_session("permission-public", model)
        .expect("create session");
    let permissions = session.permission_handle();
    assert_eq!(permissions.context().session_id, session.agent_id());
    let rule = RememberedRule {
        key: RuleKey {
            tool_name: "shell".to_owned(),
            pattern: None,
        },
        allow: true,
        scope: PermissionRuleScope::Session,
        reason: None,
    };
    let address = PermissionRuleAddress::from(&rule);

    permissions.remember_rule(rule).expect("remember rule");
    assert_eq!(session.remembered_rules().expect("list rules").len(), 1);
    assert!(permissions.revoke_rule(&address).expect("revoke rule"));
    assert_eq!(
        permissions
            .clear_scope(PermissionRuleScope::Session)
            .expect("clear empty scope"),
        0
    );
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

/// The registration shape the pointer-forwarding impls exist for, written from
/// outside the crate: `with_tool` takes a tool *by value*, so a host holding
/// one behind a pointer — a `Box<dyn ExecutableTool>` it picked at runtime, or
/// an `Arc` it wants two runtimes to share — had no public path in and had to
/// hand-forward eight methods to get one.
///
/// Two claims, and compiling is only the first. The second is that
/// `authorization_preview` survives the trip: it is defaulted, and a wrapper
/// that fell back to the default would hand the approver a preview rebuilt
/// from the descriptor instead of the one the tool wrote — the tool presenting
/// as something other than what it is. So the tool below reports a side effect
/// its descriptor does not, and the authorizer is asked what it actually saw.
#[tokio::test]
async fn a_runtime_builder_takes_a_boxed_or_shared_tool() {
    #[derive(Default)]
    struct CountingTool {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl ToolDefinition for CountingTool {
        fn descriptor(&self) -> ToolSpec {
            // Says `None` by omission, which is what the defaulted
            // `authorization_preview` would report in place of the override.
            ToolSpec::builder("counting_tool")
                .description("Count executions across every runtime holding it")
                .input_schema(json!({ "type": "object", "properties": {} }))
                .build()
        }
    }

    #[async_trait]
    impl ToolExecutor for CountingTool {
        fn authorization_preview(
            &self,
            ctx: &ParallelToolContext,
            input: &Value,
        ) -> Result<ToolAuthorizationPreview, String> {
            let descriptor = self.descriptor();
            Ok(ToolAuthorizationPreview {
                working_directory: ctx.working_directory().to_path_buf(),
                capabilities: descriptor.capabilities,
                side_effect_level: ToolSideEffectLevel::External,
                durability: descriptor.durability,
                execution_category: descriptor.execution_category,
                approval_category: descriptor.approval_category,
                raw_input: input.clone(),
                structured_input: input.clone(),
            })
        }

        async fn execute(&self, _ctx: ParallelToolContext, _input: Value) -> ToolResult {
            Ok(format!(
                "call {}",
                self.calls.fetch_add(1, Ordering::SeqCst) + 1
            ))
        }
    }

    #[derive(Clone, Default)]
    struct PreviewLog(Arc<Mutex<Vec<ToolAuthorizationPreview>>>);

    #[async_trait]
    impl ToolAuthorizer for PreviewLog {
        async fn authorize(
            &self,
            request: &ToolAuthorizationRequest,
        ) -> Result<ToolAuthorizationDecision, RuntimeError> {
            self.0
                .lock()
                .expect("preview log poisoned")
                .push(request.preview.clone());
            Ok(ToolAuthorizationDecision::allow())
        }
    }

    // The signature a host writes when the tool it registers is not known until
    // run time. Naming `ExecutableTool` from outside is part of the claim.
    fn runtime_with_tool<T>(provider: ScriptedProvider, authorizer: PreviewLog, tool: T) -> Runtime
    where
        T: ExecutableTool + 'static,
    {
        Runtime::builder()
            .with_runtime_identifier(format!("public-api-{}", now_nanos()))
            .with_store(VolatileRuntimeStore::new())
            .with_provider_instance(provider)
            .with_policy(RuntimePolicy::permissive())
            .with_tool_authorizer(authorizer)
            .with_tool(tool)
            .build()
            .expect("build runtime")
    }

    let model = ModelInfo::new("mock-model", BuiltinProvider::OpenAI);
    let previews = PreviewLog::default();

    let one_turn_calling_the_tool = |reply: &str| {
        let provider = ScriptedProvider::new(model.provider.clone(), vec![model.clone()]);
        provider.push_turns(vec![
            Turn::ToolCalls(vec![ScriptedToolCall::new("counting_tool", json!({}))]),
            Turn::Text(reply.to_string()),
        ]);
        provider
    };

    let boxed: Box<dyn ExecutableTool> = Box::new(CountingTool::default());
    let boxed_runtime = runtime_with_tool(
        one_turn_calling_the_tool("boxed done"),
        previews.clone(),
        boxed,
    );
    assert_eq!(
        boxed_runtime
            .tool_descriptor("counting_tool")
            .expect("the boxed tool registered under its own name")
            .provider
            .name,
        "counting_tool",
    );
    let mut agent = boxed_runtime
        .spawn("boxed-agent", model.clone())
        .expect("spawn boxed-tool agent");
    assert_eq!(
        agent
            .send(vec![ContentBlock::text("go")])
            .await
            .expect("boxed tool run completes")
            .text(),
        "boxed done",
    );

    // One tool instance, two runtimes — the case that previously had no public
    // path at all, leaving a host to route through shell commands instead.
    let shared = Arc::new(CountingTool::default());
    for host in ["first-host", "second-host"] {
        let runtime = runtime_with_tool(
            one_turn_calling_the_tool(&format!("{host} done")),
            previews.clone(),
            Arc::clone(&shared),
        );
        let mut agent = runtime
            .spawn(host, model.clone())
            .expect("spawn shared-tool agent");
        assert_eq!(
            agent
                .send(vec![ContentBlock::text("go")])
                .await
                .expect("shared tool run completes")
                .text(),
            format!("{host} done"),
        );
    }
    assert_eq!(
        shared.calls.load(Ordering::SeqCst),
        2,
        "both runtimes drove the same tool instance, not a copy each",
    );

    let seen = previews.0.lock().expect("preview log poisoned").clone();
    assert_eq!(seen.len(), 3, "every call was put to the authorizer");
    for preview in seen {
        assert_eq!(
            preview.side_effect_level,
            ToolSideEffectLevel::External,
            "the pointer forwarded authorization_preview instead of falling \
             back to the descriptor-derived default",
        );
    }
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
