use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::{
    BuiltinProvider, ModelInfo, Runtime, RuntimePolicy,
    error::RuntimeError,
    provider::{
        ContentBlock, Provider, ProviderDescriptor, ProviderError, ProviderEvent,
        ProviderEventStream, ProviderId, Request, Response, Role,
        provider_event_stream_from_response,
    },
    runtime::{PostExecutionHook, PreExecutionHook, VolatileRuntimeStore},
    tool::ToolAuthorizer,
};

/// Disambiguates mock runtimes built within one clock tick. The wall clock
/// alone is not a source of uniqueness: two builds can read the same
/// nanosecond, and did.
static NEXT_MOCK_RUNTIME_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub enum MockTurn {
    Text(String),
    StreamText(Vec<String>),
    ToolCalls(Vec<MockToolCall>),
    Failure(ProviderError),
}

#[derive(Debug, Clone)]
pub struct MockToolCall {
    id: Option<String>,
    name: String,
    input: Value,
}

impl MockToolCall {
    pub fn new(name: impl Into<String>, input: Value) -> Self {
        Self {
            id: None,
            name: name.into(),
            input,
        }
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }
}

/// A runtime whose provider replies from a script, for tests that need a real
/// runtime without a real model.
///
/// State lives in a [`VolatileRuntimeStore`] unless
/// [`MockRuntimeBuilder::with_store`] says otherwise: a mock writes nothing to
/// disk, and two mocks never share anything. Dropping one leaves no file to
/// clean up.
pub struct MockRuntime {
    runtime: Runtime,
    provider: ScriptedProvider,
    model: ModelInfo,
}

impl MockRuntime {
    pub fn builder() -> MockRuntimeBuilder {
        MockRuntimeBuilder::default()
    }

    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    pub fn model(&self) -> ModelInfo {
        self.model.clone()
    }

    pub async fn recorded_requests(&self) -> Vec<Request<'static>> {
        self.provider.recorded_requests()
    }
}

pub struct MockRuntimeBuilder {
    model: ModelInfo,
    turns: Vec<MockTurn>,
    runtime_identifier: String,
    #[cfg(feature = "store-sqlite")]
    store: Option<crate::runtime::SqliteRuntimeStore>,
    policy: RuntimePolicy,
    tool_authorizer: Option<Box<dyn ToolAuthorizer>>,
    pre_hook: Option<Box<dyn PreExecutionHook>>,
    post_hook: Option<Box<dyn PostExecutionHook>>,
}

impl Default for MockRuntimeBuilder {
    fn default() -> Self {
        let runtime_identifier = format!(
            "mock-runtime-{}-{}",
            now_nanos(),
            NEXT_MOCK_RUNTIME_ID.fetch_add(1, Ordering::Relaxed)
        );
        Self {
            model: ModelInfo::new("mock-model", BuiltinProvider::OpenAI),
            turns: Vec::new(),
            runtime_identifier,
            #[cfg(feature = "store-sqlite")]
            store: None,
            policy: RuntimePolicy::permissive(),
            tool_authorizer: None,
            pre_hook: None,
            post_hook: None,
        }
    }
}

impl MockRuntimeBuilder {
    pub fn model(mut self, id: impl Into<String>, provider: impl Into<ProviderId>) -> Self {
        self.model = ModelInfo::new(id.into(), provider.into());
        self
    }

    pub fn runtime_identifier(mut self, runtime_identifier: impl Into<String>) -> Self {
        self.runtime_identifier = runtime_identifier.into();
        self
    }

    /// Runs the scripted runtime against a SQLite store instead of the
    /// volatile default, for a test that needs state to outlive the
    /// `MockRuntime` — reopening the same path from a second runtime to
    /// exercise resume, or inspecting the database directly.
    ///
    /// The caller owns the path, and therefore owns cleaning it up. The
    /// default leaves nothing to clean up.
    #[cfg(feature = "store-sqlite")]
    pub fn with_store(mut self, store: crate::runtime::SqliteRuntimeStore) -> Self {
        self.store = Some(store);
        self
    }

    /// Replaces the runtime policy used by the scripted runtime.
    pub fn with_policy(mut self, policy: RuntimePolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Installs a pre-execution hook, so a scripted run can exercise the
    /// interception path.
    ///
    /// The sibling of [`with_tool_authorizer`](Self::with_tool_authorizer):
    /// without one, nothing ever consults a hook, so a host can test that its
    /// own hook logic is correct but not that the runtime actually calls it.
    pub fn with_pre_hook(mut self, hook: impl PreExecutionHook + 'static) -> Self {
        self.pre_hook = Some(Box::new(hook));
        self
    }

    /// Installs a post-execution hook, so a scripted run can exercise the
    /// result-rewriting path.
    ///
    /// The same reason [`with_pre_hook`](Self::with_pre_hook) exists: a host
    /// can unit-test its own hook, but only a scripted runtime shows that the
    /// runtime consults it and honors what it returned.
    pub fn with_post_hook(mut self, hook: impl PostExecutionHook + 'static) -> Self {
        self.post_hook = Some(Box::new(hook));
        self
    }

    /// Installs a tool authorizer, so a scripted run can exercise the
    /// permission flow.
    ///
    /// Without one the session authorizer allows every call unconditionally
    /// and [`SessionEvent::PermissionRequested`](crate::SessionEvent) is never
    /// emitted — which makes "does this host ask before it writes?" impossible
    /// to test against a mock.
    pub fn with_tool_authorizer(mut self, authorizer: impl ToolAuthorizer + 'static) -> Self {
        self.tool_authorizer = Some(Box::new(authorizer));
        self
    }

    /// Gives the scripted model a context window, as a provider listing would.
    ///
    /// The window is what a window-relative compaction threshold is computed
    /// from, so a test covering that behavior needs a model that reports one.
    pub fn model_context_window(mut self, context_window: usize) -> Self {
        self.model.context_window = Some(context_window);
        self
    }

    pub fn push_turn(mut self, turn: MockTurn) -> Self {
        self.turns.push(turn);
        self
    }

    pub fn text(self, text: impl Into<String>) -> Self {
        self.push_turn(MockTurn::Text(text.into()))
    }

    pub fn stream_text<I, S>(self, chunks: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.push_turn(MockTurn::StreamText(
            chunks.into_iter().map(Into::into).collect(),
        ))
    }

    pub fn tool_calls<I>(self, calls: I) -> Self
    where
        I: IntoIterator<Item = MockToolCall>,
    {
        self.push_turn(MockTurn::ToolCalls(calls.into_iter().collect()))
    }

    pub fn failure(self, error: ProviderError) -> Self {
        self.push_turn(MockTurn::Failure(error))
    }

    pub fn build(self) -> Result<MockRuntime, RuntimeError> {
        let provider = ScriptedProvider::new(self.model.provider.clone(), vec![self.model.clone()]);
        provider.push_turns(self.turns);

        let mut builder = Runtime::builder()
            .with_runtime_identifier(self.runtime_identifier)
            .with_policy(self.policy)
            .with_provider_instance(provider.clone());

        // A scripted runtime is ephemeral by definition, so its store is too.
        // The default used to be a SQLite file named after the current
        // nanosecond in the system temp directory, which left one file behind
        // per mock and — when two mocks read the same tick — handed both the
        // same database, where the second one's agent lease was already held.
        #[cfg(feature = "store-sqlite")]
        {
            builder = match self.store {
                Some(store) => builder.with_store(store),
                None => builder.with_store(VolatileRuntimeStore::new()),
            };
        }
        #[cfg(not(feature = "store-sqlite"))]
        {
            builder = builder.with_store(VolatileRuntimeStore::new());
        }

        if let Some(hook) = self.pre_hook {
            builder = builder.with_pre_hook(hook);
        }

        if let Some(hook) = self.post_hook {
            builder = builder.with_post_hook(hook);
        }

        if let Some(authorizer) = self.tool_authorizer {
            builder = builder.with_tool_authorizer(authorizer);
        }

        let runtime = builder.build()?;

        Ok(MockRuntime {
            runtime,
            provider,
            model: self.model,
        })
    }
}

#[derive(Clone)]
struct ScriptedProvider {
    kind: ProviderId,
    models: Vec<ModelInfo>,
    turns: Arc<Mutex<VecDeque<MockTurn>>>,
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

    fn push_turns(&self, turns: Vec<MockTurn>) {
        let mut queue = self.turns.lock().expect("mock turn queue poisoned");
        queue.extend(turns);
    }

    fn recorded_requests(&self) -> Vec<Request<'static>> {
        self.requests
            .lock()
            .expect("mock request log poisoned")
            .clone()
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(self.kind.clone())
    }

    /// A scripted provider does list its models, and saying so is what lets a
    /// pinned model id resolve through the listing the way a real one does.
    fn capabilities(&self) -> crate::provider::ProviderCapabilities {
        crate::provider::ProviderCapabilities {
            supports_model_listing: true,
            supports_streaming: true,
            supports_tool_calls: true,
            ..Default::default()
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(self.models.clone())
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        self.requests
            .lock()
            .expect("mock request log poisoned")
            .push(request.into_owned());
        let turn = self
            .turns
            .lock()
            .expect("mock turn queue poisoned")
            .pop_front()
            .unwrap_or_else(|| panic!("no scripted turn remaining for mock runtime"));

        match turn {
            MockTurn::Text(text) => Ok(response_stream(
                &self.models[0],
                Response {
                    id: format!("mock-response-{}", now_nanos()),
                    model: self.models[0].id.clone(),
                    role: Role::Assistant,
                    content: vec![ContentBlock::text(text)],
                    stop_reason: None,
                    usage: None,
                },
            )),
            MockTurn::StreamText(chunks) => Ok(streaming_text_response(&self.models[0], chunks)),
            MockTurn::ToolCalls(calls) => Ok(response_stream(
                &self.models[0],
                Response {
                    id: format!("mock-response-{}", now_nanos()),
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
                },
            )),
            MockTurn::Failure(error) => Err(error),
        }
    }
}

fn response_stream(model: &ModelInfo, response: Response) -> ProviderEventStream {
    let _ = model;
    provider_event_stream_from_response(response)
}

fn streaming_text_response(model: &ModelInfo, chunks: Vec<String>) -> ProviderEventStream {
    let (tx, rx) = mpsc::unbounded_channel();

    tx.send(Ok(ProviderEvent::MessageStarted {
        id: format!("mock-response-{}", now_nanos()),
        model: model.id.clone(),
        role: Role::Assistant,
    }))
    .expect("mock runtime message start receiver dropped");
    tx.send(Ok(ProviderEvent::ContentBlockStarted {
        index: 0,
        kind: crate::provider::ContentBlockStart::Text,
    }))
    .expect("mock runtime content start receiver dropped");

    for chunk in chunks {
        tx.send(Ok(ProviderEvent::ContentBlockDelta {
            index: 0,
            delta: crate::provider::ContentBlockDelta::Text(chunk),
        }))
        .expect("mock runtime content delta receiver dropped");
    }

    tx.send(Ok(ProviderEvent::ContentBlockStopped { index: 0 }))
        .expect("mock runtime content stop receiver dropped");
    tx.send(Ok(ProviderEvent::MessageDelta {
        stop_reason: None,
        usage: None,
    }))
    .expect("mock runtime message delta receiver dropped");
    tx.send(Ok(ProviderEvent::MessageStopped))
        .expect("mock runtime message stop receiver dropped");

    rx
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        Agent,
        agent::{AgentConfig, ToolProfile},
        provider::Message,
        tool::{ParallelToolContext, ToolDefinition, ToolExecutor, ToolResult, ToolSpec},
    };

    struct EchoTool;

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

    async fn spawn_agent(mock: &MockRuntime) -> Agent {
        mock.runtime()
            .spawn("mock-agent", mock.model())
            .expect("spawn mock agent")
    }

    #[tokio::test]
    async fn mock_runtime_replays_text_turns() {
        let mock = MockRuntime::builder()
            .text("hello from mock")
            .build()
            .unwrap();
        let mut agent = spawn_agent(&mock).await;

        let message = agent.send(vec![ContentBlock::text("hi")]).await.unwrap();

        assert_eq!(
            message,
            Message::assistant(ContentBlock::text("hello from mock"))
        );
    }

    #[tokio::test]
    async fn mock_runtime_replays_streaming_text_turns() {
        let mock = MockRuntime::builder()
            .stream_text(["hello", " ", "world"])
            .build()
            .unwrap();
        let mut agent = spawn_agent(&mock).await;

        let message = agent.send(vec![ContentBlock::text("hi")]).await.unwrap();

        assert_eq!(message.text(), "hello world");
    }

    #[tokio::test]
    async fn mock_runtime_surfaces_provider_failures() {
        let mock = MockRuntime::builder()
            .failure(ProviderError::InvalidResponse("boom".to_string()))
            .build()
            .unwrap();
        let mut agent = spawn_agent(&mock).await;

        let error = agent
            .send(vec![ContentBlock::text("hi")])
            .await
            .unwrap_err();

        assert!(matches!(error, RuntimeError::FailedToStreamResponse(_)));
    }

    #[tokio::test]
    async fn mock_runtime_can_script_tool_call_turns() {
        let mock = MockRuntime::builder()
            .tool_calls([MockToolCall::new("echo_tool", json!({}))])
            .text("done")
            .build()
            .unwrap();
        mock.runtime().register_tool(EchoTool);
        let mut agent = spawn_agent(&mock).await;

        let message = agent
            .send(vec![ContentBlock::text("run the tool")])
            .await
            .unwrap();

        assert_eq!(message.text(), "done");
        assert_eq!(mock.recorded_requests().await.len(), 2);
    }

    #[tokio::test]
    async fn mock_runtime_supports_runtime_assembly_assertions() {
        let mock = MockRuntime::builder().text("done").build().unwrap();
        mock.runtime().register_tool(EchoTool);
        let mut agent = mock
            .runtime()
            .spawn_with_config(
                "mock-agent",
                mock.model(),
                AgentConfig {
                    tool_profile: ToolProfile::only(["echo_tool"]),
                    ..Default::default()
                },
            )
            .expect("spawn mock agent");

        let message = agent.send(vec![ContentBlock::text("hi")]).await.unwrap();

        assert_eq!(message.text(), "done");

        let requests = mock.recorded_requests().await;
        let tool_names = requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(tool_names, vec!["echo_tool"]);
    }

    /// An authorizer that prompts for everything, so the session authorizer
    /// has something to raise.
    struct AlwaysPrompts;

    #[async_trait]
    impl crate::tool::ToolAuthorizer for AlwaysPrompts {
        async fn authorize(
            &self,
            _request: &crate::tool::ToolAuthorizationRequest,
        ) -> Result<crate::tool::ToolAuthorizationDecision, RuntimeError> {
            Ok(crate::tool::ToolAuthorizationDecision::prompt("ask first"))
        }
    }

    #[tokio::test]
    async fn a_mock_runtime_can_exercise_the_permission_path() {
        let mock = MockRuntime::builder()
            .with_tool_authorizer(AlwaysPrompts)
            // A builtin, so the call really reaches the tool layer and
            // therefore the authorizer.
            .tool_calls(vec![MockToolCall::new(
                "files",
                json!({"operations": [{"op": "list", "path": "."}]}),
            )])
            .text("done")
            .build()
            .unwrap();

        let mut session = mock.runtime().create_session("test", mock.model()).unwrap();

        let mut events = session.subscribe();
        let permissions = session.permission_handle();
        let asked = Arc::new(Mutex::new(false));
        let saw = Arc::clone(&asked);

        // Answer whatever is asked, so the turn is not left blocked forever.
        let watcher = tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                if let crate::SessionEvent::PermissionRequested { request_id, .. } = event {
                    *saw.lock().unwrap() = true;
                    let _ = permissions.resolve_permission(
                        &request_id,
                        crate::session::PermissionDecision::deny(),
                    );
                }
            }
        });

        let _ = session.append_turn(vec![ContentBlock::text("go")]).await;
        watcher.abort();

        assert!(
            *asked.lock().unwrap(),
            "an authorizer installed on the mock must reach the session's permission flow"
        );
    }

    /// Denies one named tool, so a scripted run can prove the runtime really
    /// consults the hook rather than that the hook's own logic is correct.
    struct DenyTool(&'static str);

    #[async_trait]
    impl crate::runtime::PreExecutionHook for DenyTool {
        async fn pre_tool_execution(
            &self,
            context: &crate::runtime::PreExecutionContext,
        ) -> Result<crate::runtime::HookDecision, RuntimeError> {
            if context.tool_name == self.0 {
                Ok(crate::runtime::HookDecision::Deny(
                    "not this one".to_string(),
                ))
            } else {
                Ok(crate::runtime::HookDecision::Allow)
            }
        }
    }

    #[tokio::test]
    async fn a_mock_runtime_can_exercise_the_interception_path() {
        let mock = MockRuntime::builder()
            .with_pre_hook(DenyTool("files"))
            .tool_calls(vec![MockToolCall::new(
                "files",
                json!({"operations": [{"op": "list", "path": "."}]}),
            )])
            .text("done")
            .build()
            .unwrap();

        let mut session = mock.runtime().create_session("test", mock.model()).unwrap();

        let _ = session.append_turn(vec![ContentBlock::text("go")]).await;

        let blocked = session.replay().items().iter().any(|item| {
            item.message.as_ref().is_some_and(|message| {
                message.content.iter().any(|block| {
                    matches!(block, ContentBlock::ToolResult { content, .. }
                        if content.to_string().contains("not this one"))
                })
            })
        });

        assert!(
            blocked,
            "a hook installed on the mock must actually be consulted by the runtime"
        );
    }

    /// How many mock-runtime databases the system temp directory holds right
    /// now. Counted as a delta rather than asserted at zero, because a machine
    /// that ran the old default has thousands of them left over and the point
    /// is that this run adds none.
    fn mock_runtime_files_in_temp() -> usize {
        let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
            return 0;
        };
        entries
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("mentra-mock-runtime")
            })
            .count()
    }

    /// The default store used to be a SQLite file in the system temp
    /// directory, one per mock, named after the current nanosecond and never
    /// deleted. A full downstream suite left dozens behind per run; one dev
    /// machine had accumulated 38,782.
    #[tokio::test]
    async fn a_default_mock_runtime_writes_nothing_to_disk() {
        let before = mock_runtime_files_in_temp();

        let mock = MockRuntime::builder().text("hello").build().unwrap();
        let mut agent = spawn_agent(&mock).await;
        agent
            .send(vec![ContentBlock::text("hi")])
            .await
            .expect("a scripted turn completes without a store on disk");

        assert_eq!(
            mock_runtime_files_in_temp(),
            before,
            "a mock runtime must leave nothing in {}",
            std::env::temp_dir().display()
        );
    }

    /// Two mocks built inside one nanosecond used to be handed the same
    /// database file. Agent ids are unique only within a process, so two test
    /// binaries running concurrently could mint the same id against that
    /// shared file — and the second `spawn` failed with `LeaseUnavailable`,
    /// the mechanism behind a downstream flake nobody could reproduce.
    /// Independent stores make the collision unreachable rather than rare.
    #[tokio::test]
    async fn mock_runtimes_built_back_to_back_do_not_share_a_store() {
        let first = MockRuntime::builder()
            .runtime_identifier("shared-identifier")
            .text("from the first")
            .build()
            .unwrap();
        let second = MockRuntime::builder()
            .runtime_identifier("shared-identifier")
            .text("from the second")
            .build()
            .unwrap();

        // Both spawns take an agent lease. Against one shared store, the
        // second is the one that would be refused.
        let mut first_agent = spawn_agent(&first).await;
        let mut second_agent = spawn_agent(&second).await;

        assert_eq!(
            first_agent
                .send(vec![ContentBlock::text("hi")])
                .await
                .expect("the first mock runs")
                .text(),
            "from the first"
        );
        assert_eq!(
            second_agent
                .send(vec![ContentBlock::text("hi")])
                .await
                .expect("the second mock runs")
                .text(),
            "from the second"
        );

        // Same runtime identifier, so anything they shared would show up here.
        for mock in [&first, &second] {
            let agents = mock
                .runtime()
                .list_persisted_agents("shared-identifier")
                .expect("lists persisted agents");
            assert_eq!(
                agents.len(),
                1,
                "each mock keeps its own store, so it sees only its own agent"
            );
        }
    }
}
