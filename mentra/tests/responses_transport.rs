//! Which transport a runtime's Responses requests go out on, and what happens
//! when a provider cannot serve the one it was handed.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mentra::{
    AgentConfig, BuiltinProvider, ContentBlock, ProviderCapabilities, Role, Runtime,
    provider::{
        ContentBlockDelta, ContentBlockStart, ModelInfo, Provider, ProviderDescriptor,
        ProviderError, ProviderEvent, ProviderEventStream, ProviderId, ProviderRequestOptions,
        Request, ResponsesTransport,
    },
    runtime::VolatileRuntimeStore,
};
use tokio::sync::mpsc;

/// Answers one short text turn and remembers the options it was handed.
///
/// The options are what this file is about: the transport is settled before the
/// request leaves the runtime, so what arrives here is the only evidence of
/// which one was chosen.
#[derive(Clone)]
struct RecordingProvider {
    id: ProviderId,
    display_name: Option<String>,
    capabilities: ProviderCapabilities,
    seen: Arc<Mutex<Vec<ProviderRequestOptions>>>,
}

impl RecordingProvider {
    fn new(id: impl Into<ProviderId>, supports_websockets: bool) -> Self {
        Self {
            id: id.into(),
            display_name: None,
            capabilities: ProviderCapabilities {
                supports_streaming: true,
                supports_websockets,
                ..ProviderCapabilities::default()
            },
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn named(mut self, display_name: &str) -> Self {
        self.display_name = Some(display_name.to_string());
        self
    }

    fn transports(&self) -> Vec<ResponsesTransport> {
        self.seen
            .lock()
            .expect("recorded options")
            .iter()
            .map(|options| options.responses.transport)
            .collect()
    }
}

#[async_trait]
impl Provider for RecordingProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: self.id.clone(),
            display_name: self.display_name.clone(),
            description: None,
        }
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![ModelInfo::new("model", self.id.clone())])
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        self.seen
            .lock()
            .expect("recorded options")
            .push(request.provider_request_options.clone());

        let (tx, rx) = mpsc::unbounded_channel();
        for event in [
            ProviderEvent::MessageStarted {
                id: "msg-1".to_string(),
                model: "model".to_string(),
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
        ] {
            tx.send(Ok(event)).expect("test receiver alive");
        }
        Ok(rx)
    }
}

/// Transport selection needs no persistence: an isolated in-memory store per
/// runtime keeps these tests independent and leaves nothing behind.
fn temp_store() -> VolatileRuntimeStore {
    VolatileRuntimeStore::new()
}

/// A runtime around `provider`, optionally told which transport to use.
fn runtime_for(provider: RecordingProvider, transport: Option<ResponsesTransport>) -> Runtime {
    let mut builder = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_store(temp_store());
    if let Some(transport) = transport {
        builder = builder.with_responses_transport(transport);
    }
    builder.build().expect("build runtime")
}

#[tokio::test]
async fn a_runtime_that_chooses_nothing_still_streams_over_http_sse() {
    let provider = RecordingProvider::new(BuiltinProvider::OpenAI, true);
    let recorder = provider.clone();
    let runtime = runtime_for(provider, None);

    let mut agent = runtime
        .spawn("agent", ModelInfo::new("model", BuiltinProvider::OpenAI))
        .expect("spawn agent");
    agent
        .send(vec![ContentBlock::text("hello")])
        .await
        .expect("the turn runs");

    assert_eq!(recorder.transports(), vec![ResponsesTransport::HttpSse]);
}

#[test]
fn a_runtime_reports_the_transport_it_was_given() {
    // Without this reader the choice is write-only: the only evidence a host's
    // selection reached the runtime is a turn run against a provider that
    // records what it was handed, so anything downstream can test its own field
    // and stop at the seam — the shape of test that passes while the wiring
    // between the two is broken.
    let unset = runtime_for(RecordingProvider::new(BuiltinProvider::OpenAI, true), None);
    assert_eq!(unset.responses_transport(), None);

    let chosen = runtime_for(
        RecordingProvider::new(BuiltinProvider::OpenAI, true),
        Some(ResponsesTransport::WebSocket),
    );
    assert_eq!(
        chosen.responses_transport(),
        Some(ResponsesTransport::WebSocket)
    );
}

#[tokio::test]
async fn a_chosen_websocket_transport_reaches_the_request() {
    // The gap this closes: `ResponsesRequestOptions.transport` existed and the
    // websocket path was compiled in, but nothing in the runtime ever set the
    // field, so every request went out over HTTP+SSE whatever the host wanted.
    let provider = RecordingProvider::new(BuiltinProvider::OpenAI, true);
    let recorder = provider.clone();
    let runtime = runtime_for(provider, Some(ResponsesTransport::WebSocket));

    let mut agent = runtime
        .spawn("agent", ModelInfo::new("model", BuiltinProvider::OpenAI))
        .expect("spawn agent");
    agent
        .send(vec![ContentBlock::text("hello")])
        .await
        .expect("the turn runs");

    assert_eq!(recorder.transports(), vec![ResponsesTransport::WebSocket]);
}

#[tokio::test]
async fn a_runtime_choice_settles_a_disagreeing_agent_config() {
    // Two live opinions about one socket is not a state the runtime keeps: the
    // connection-level answer is the one that holds.
    let provider = RecordingProvider::new(BuiltinProvider::OpenAI, true);
    let recorder = provider.clone();
    let runtime = runtime_for(provider, Some(ResponsesTransport::HttpSse));

    let mut config = AgentConfig::default();
    config.provider_request_options.responses.transport = ResponsesTransport::WebSocket;
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            ModelInfo::new("model", BuiltinProvider::OpenAI),
            config,
        )
        .expect("spawn agent");
    agent
        .send(vec![ContentBlock::text("hello")])
        .await
        .expect("the turn runs");

    assert_eq!(recorder.transports(), vec![ResponsesTransport::HttpSse]);
}

#[tokio::test]
async fn a_provider_without_websockets_refuses_rather_than_pretending() {
    // anthropic and gemini report `supports_websockets: false`. Answering over
    // HTTP+SSE would look like success and be a transport nobody asked for.
    let provider = RecordingProvider::new(BuiltinProvider::Anthropic, false).named("Anthropic");
    let recorder = provider.clone();
    let runtime = runtime_for(provider, Some(ResponsesTransport::WebSocket));

    let mut agent = runtime
        .spawn("agent", ModelInfo::new("model", BuiltinProvider::Anthropic))
        .expect("spawn agent");
    let error = agent
        .send(vec![ContentBlock::text("hello")])
        .await
        .expect_err("a transport the provider cannot serve is refused");

    let message = error.to_string();
    assert!(
        message.contains("Anthropic"),
        "the refusal must name the provider: {message}"
    );
    assert!(
        message.contains("websocket"),
        "and say what it could not do: {message}"
    );
    assert!(
        recorder.transports().is_empty(),
        "the request must never have been sent"
    );
}
