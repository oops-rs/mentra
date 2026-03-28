use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
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
    runtime::SqliteRuntimeStore,
};

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
}

impl Default for MockRuntimeBuilder {
    fn default() -> Self {
        let runtime_identifier = format!("mock-runtime-{}", now_nanos());
        Self {
            model: ModelInfo::new("mock-model", BuiltinProvider::OpenAI),
            turns: Vec::new(),
            runtime_identifier,
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

        let store_path =
            std::env::temp_dir().join(format!("mentra-mock-runtime-{}.sqlite", now_nanos()));

        let runtime = Runtime::builder()
            .with_runtime_identifier(self.runtime_identifier)
            .with_store(SqliteRuntimeStore::new(store_path))
            .with_policy(RuntimePolicy::permissive())
            .with_provider_instance(provider.clone())
            .build()?;

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
}
