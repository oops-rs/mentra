use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use mentra::error::RuntimeError;
use mentra::provider::{
    CompactionRequest, CompactionResponse, MemorySummarizeOutput, MemorySummarizeRequest,
    MemorySummarizeResponse, Provider, ProviderCapabilities, ProviderDescriptor, ProviderError,
    ProviderEventStream, ProviderId, ProviderRequestOptions, Request, Response,
};
use mentra::runtime::{ErrorCategory, VolatileRuntimeStore};
use mentra::{
    Agent, BuiltinProvider, ContentBlock, Message, ModelInfo, ProviderSessionScope, Role, Runtime,
    collect_response_from_stream, provider_event_stream_from_response,
};

const MODEL: &str = "scope-model";

#[derive(Default)]
struct ProbeCalls {
    list_models: AtomicUsize,
    stream: AtomicUsize,
    send: AtomicUsize,
    compact: AtomicUsize,
    summarize: AtomicUsize,
    fresh: AtomicUsize,
}

#[derive(Clone)]
struct ScopedProbeProvider {
    id: ProviderId,
    generation: usize,
    next_generation: Arc<AtomicUsize>,
    turns: Arc<AtomicUsize>,
    calls: Arc<ProbeCalls>,
}

impl ScopedProbeProvider {
    fn new(id: impl Into<ProviderId>) -> Self {
        Self {
            id: id.into(),
            generation: 0,
            next_generation: Arc::new(AtomicUsize::new(0)),
            turns: Arc::new(AtomicUsize::new(0)),
            calls: Arc::new(ProbeCalls::default()),
        }
    }

    fn model(&self) -> ModelInfo {
        ModelInfo::new(MODEL, self.id.clone())
    }

    fn response(&self, text: impl Into<String>) -> Response {
        Response {
            id: format!("scope-response-{}", self.generation),
            model: MODEL.to_string(),
            role: Role::Assistant,
            content: vec![ContentBlock::text(text)],
            stop_reason: Some("stop".to_string()),
            usage: None,
        }
    }
}

#[async_trait]
impl Provider for ScopedProbeProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        let mut descriptor = ProviderDescriptor::new(self.id.clone());
        descriptor.display_name = Some("Scoped probe".to_string());
        descriptor
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_streaming: true,
            supports_history_compaction: true,
            supports_memory_summarization: true,
            ..ProviderCapabilities::default()
        }
    }

    fn fresh_session_scope(&self) -> Result<ProviderSessionScope, ProviderError> {
        self.calls.fresh.fetch_add(1, Ordering::SeqCst);
        let generation = self.next_generation.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(ProviderSessionScope::new(Self {
            id: self.id.clone(),
            generation,
            next_generation: Arc::clone(&self.next_generation),
            turns: Arc::new(AtomicUsize::new(0)),
            calls: Arc::clone(&self.calls),
        }))
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.calls.list_models.fetch_add(1, Ordering::SeqCst);
        Ok(vec![self.model()])
    }

    async fn stream(&self, _request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        self.calls.stream.fetch_add(1, Ordering::SeqCst);
        let turn = self.turns.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(provider_event_stream_from_response(self.response(format!(
            "generation={} turn={turn}",
            self.generation
        ))))
    }

    async fn send(&self, _request: Request<'_>) -> Result<Response, ProviderError> {
        self.calls.send.fetch_add(1, Ordering::SeqCst);
        Ok(self.response(format!("send generation={}", self.generation)))
    }

    async fn compact(
        &self,
        _request: CompactionRequest<'_>,
    ) -> Result<CompactionResponse, ProviderError> {
        self.calls.compact.fetch_add(1, Ordering::SeqCst);
        Ok(CompactionResponse::from_text(format!(
            "compact generation={}",
            self.generation
        )))
    }

    async fn summarize_memories(
        &self,
        _request: MemorySummarizeRequest<'_>,
    ) -> Result<MemorySummarizeResponse, ProviderError> {
        self.calls.summarize.fetch_add(1, Ordering::SeqCst);
        Ok(MemorySummarizeResponse {
            output: vec![MemorySummarizeOutput {
                raw_memory: format!("raw generation={}", self.generation),
                memory_summary: "summary".to_string(),
            }],
        })
    }
}

struct OneShotProvider {
    id: ProviderId,
}

#[async_trait]
impl Provider for OneShotProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(self.id.clone())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![ModelInfo::new(MODEL, self.id.clone())])
    }

    async fn stream(&self, _request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Ok(rx)
    }
}

struct MismatchedProvider;

#[async_trait]
impl Provider for MismatchedProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new("source-provider")
    }

    fn fresh_session_scope(&self) -> Result<ProviderSessionScope, ProviderError> {
        Ok(ProviderSessionScope::new(OneShotProvider {
            id: ProviderId::new("different-provider"),
        }))
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![ModelInfo::new(MODEL, "source-provider")])
    }

    async fn stream(&self, _request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Ok(rx)
    }
}

fn runtime_with<P>(provider: P) -> Runtime
where
    P: Provider + 'static,
{
    Runtime::empty_builder()
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .build()
        .expect("provider forms a runtime")
}

fn request() -> Request<'static> {
    Request {
        model: Cow::Borrowed(MODEL),
        system: None,
        messages: Cow::Owned(vec![Message::user(ContentBlock::text("probe"))]),
        tools: Cow::Owned(Vec::new()),
        tool_choice: None,
        temperature: None,
        max_output_tokens: None,
        metadata: Cow::Owned(BTreeMap::new()),
        provider_request_options: ProviderRequestOptions::default(),
    }
}

fn compaction_request() -> CompactionRequest<'static> {
    CompactionRequest {
        model: Cow::Borrowed(MODEL),
        instructions: Cow::Borrowed("compact"),
        input: Cow::Owned(Vec::new()),
        metadata: Cow::Owned(BTreeMap::new()),
        provider_request_options: ProviderRequestOptions::default(),
    }
}

fn memory_request() -> MemorySummarizeRequest<'static> {
    MemorySummarizeRequest {
        model: Cow::Borrowed(MODEL),
        raw_memories: Cow::Owned(Vec::new()),
        reasoning: None,
        metadata: Cow::Owned(BTreeMap::new()),
        provider_request_options: ProviderRequestOptions::default(),
    }
}

async fn next_text(agent: &mut Agent) -> String {
    agent
        .send(vec![ContentBlock::text("next")])
        .await
        .expect("scope turn completes")
        .text()
}

fn response_text(response: &Response) -> String {
    response
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[test]
fn default_and_explicit_provider_lookup_mint_the_requested_scope() {
    let default = ScopedProbeProvider::new("default-provider");
    let explicit = ScopedProbeProvider::new("explicit-provider");
    let runtime = Runtime::empty_builder()
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(default)
        .with_provider_instance(explicit)
        .build()
        .expect("two providers form a runtime");

    let default_scope = runtime
        .fresh_provider_session_scope(None)
        .expect("default provider scope");
    let explicit_scope = runtime
        .fresh_provider_session_scope(Some(&ProviderId::new("explicit-provider")))
        .expect("explicit provider scope");

    assert_eq!(default_scope.descriptor().id.as_str(), "default-provider");
    assert_eq!(explicit_scope.descriptor().id.as_str(), "explicit-provider");
}

#[test]
fn a_missing_provider_is_reported_before_scope_creation() {
    let runtime = runtime_with(ScopedProbeProvider::new("registered"));
    let missing = ProviderId::new("missing");

    let error = runtime
        .fresh_provider_session_scope(Some(&missing))
        .err()
        .expect("missing provider must fail");

    assert!(matches!(
        error,
        RuntimeError::ProviderNotFound(Some(provider)) if provider == missing
    ));
}

#[test]
fn a_custom_provider_keeps_compiling_and_reports_unsupported_by_default() {
    let runtime = runtime_with(OneShotProvider {
        id: ProviderId::new("one-shot"),
    });

    let error = runtime
        .fresh_provider_session_scope(None)
        .err()
        .expect("one-shot provider cannot mint a scope");

    assert_eq!(error.category(), ErrorCategory::Terminal);
    assert!(matches!(
        error,
        RuntimeError::FailedToCreateProviderSessionScope(
            ProviderError::UnsupportedCapability(capability)
        ) if capability == "fresh_session_scope"
    ));
}

#[test]
fn a_provider_cannot_change_identity_while_minting_a_scope() {
    let runtime = runtime_with(MismatchedProvider);

    let error = runtime
        .fresh_provider_session_scope(None)
        .err()
        .expect("identity drift must fail");

    assert_eq!(error.category(), ErrorCategory::Terminal);
    assert!(matches!(
        error,
        RuntimeError::ProviderSessionScopeIdentityMismatch { expected, actual }
            if expected == ProviderId::new("source-provider")
                && actual == ProviderId::new("different-provider")
    ));
}

#[test]
fn every_builtin_and_compatible_facade_mints_without_contacting_its_endpoint() {
    for provider in [
        BuiltinProvider::Anthropic,
        BuiltinProvider::Gemini,
        BuiltinProvider::OpenAI,
        BuiltinProvider::OpenRouter,
        BuiltinProvider::Ollama,
        BuiltinProvider::LmStudio,
    ] {
        let runtime = Runtime::empty_builder()
            .with_store(VolatileRuntimeStore::new())
            .with_provider(provider, "unused-key")
            .build()
            .expect("builtin forms a runtime");

        let scope = runtime
            .fresh_provider_session_scope(None)
            .expect("builtin mints a local fresh scope");

        assert_eq!(scope.descriptor().id, ProviderId::from(provider));
    }

    let compatible = Runtime::empty_builder()
        .with_store(VolatileRuntimeStore::new())
        .with_openai_compatible(
            "custom-compatible",
            "http://127.0.0.1:1/",
            Some("unused-key".to_string()),
        )
        .build()
        .expect("compatible facade forms a runtime");
    assert_eq!(
        compatible
            .fresh_provider_session_scope(None)
            .expect("compatible facade mints a local fresh scope")
            .descriptor()
            .id,
        ProviderId::new("custom-compatible")
    );
}

#[test]
fn registered_provider_and_provider_core_scope_both_cross_the_facade() {
    let mut definition = mentra::provider_core::responses::openai_definition();
    definition.descriptor.id = ProviderId::new("registered-responses");
    definition.base_url = Some("http://127.0.0.1:1/".to_string());
    let provider = mentra::provider_core::responses::ResponsesProvider::new(
        definition,
        mentra::provider_core::StaticCredentialSource::new("unused-key"),
    );
    let core_scope = mentra::provider_core::Provider::fresh_session_scope(&provider)
        .expect("provider-core mints a scope");
    let runtime = Runtime::empty_builder()
        .with_store(VolatileRuntimeStore::new())
        .with_registered_provider(core_scope)
        .build()
        .expect("provider-core scope registers");

    let high_scope = runtime
        .fresh_provider_session_scope(None)
        .expect("registered proxy delegates freshness");
    let rebuilt = runtime_with(high_scope);

    assert_eq!(
        rebuilt.providers()[0].id,
        ProviderId::new("registered-responses")
    );
}

async fn assert_source_and_fresh_scopes_are_isolated(fresh_first: bool) {
    let provider = ScopedProbeProvider::new("isolated");
    let model = provider.model();
    let source_runtime = runtime_with(provider);
    let fresh_scope = source_runtime
        .fresh_provider_session_scope(None)
        .expect("fresh runtime provider");
    let fresh_runtime = runtime_with(fresh_scope);
    let mut source = source_runtime
        .spawn("source", model.clone())
        .expect("spawn source");
    let mut fresh = fresh_runtime.spawn("fresh", model).expect("spawn fresh");

    if fresh_first {
        assert_eq!(next_text(&mut fresh).await, "generation=1 turn=1");
        assert_eq!(next_text(&mut source).await, "generation=0 turn=1");
        assert_eq!(next_text(&mut fresh).await, "generation=1 turn=2");
        assert_eq!(next_text(&mut source).await, "generation=0 turn=2");
    } else {
        assert_eq!(next_text(&mut source).await, "generation=0 turn=1");
        assert_eq!(next_text(&mut fresh).await, "generation=1 turn=1");
        assert_eq!(next_text(&mut source).await, "generation=0 turn=2");
        assert_eq!(next_text(&mut fresh).await, "generation=1 turn=2");
    }
}

#[tokio::test]
async fn source_then_fresh_runtime_state_is_isolated() {
    assert_source_and_fresh_scopes_are_isolated(false).await;
}

#[tokio::test]
async fn fresh_then_source_runtime_state_is_isolated() {
    assert_source_and_fresh_scopes_are_isolated(true).await;
}

#[tokio::test]
async fn a_returned_scope_clone_shares_state_and_another_fresh_scope_splits_it() {
    let source = runtime_with(ScopedProbeProvider::new("clone-semantics"));
    let scope = source
        .fresh_provider_session_scope(None)
        .expect("first fresh scope");
    let shared = scope.clone();
    let split = scope
        .fresh_session_scope()
        .expect("freshening the wrapper delegates to its provider");
    let model = ModelInfo::new(MODEL, "clone-semantics");
    let first_runtime = runtime_with(scope);
    let shared_runtime = runtime_with(shared);
    let split_runtime = runtime_with(split);
    let mut first = first_runtime
        .spawn("first", model.clone())
        .expect("spawn first");
    let mut shared = shared_runtime
        .spawn("shared", model.clone())
        .expect("spawn shared");
    let mut split = split_runtime.spawn("split", model).expect("spawn split");

    assert_eq!(next_text(&mut first).await, "generation=1 turn=1");
    assert_eq!(next_text(&mut shared).await, "generation=1 turn=2");
    assert_eq!(next_text(&mut split).await, "generation=2 turn=1");
}

#[tokio::test]
async fn the_public_scope_delegates_the_complete_provider_contract() {
    fn accepts_provider_object(_provider: &dyn Provider) {}

    let provider = ScopedProbeProvider::new("delegation");
    let calls = Arc::clone(&provider.calls);
    let scope = ProviderSessionScope::new(provider);
    accepts_provider_object(&scope);

    assert_eq!(
        scope.descriptor().display_name.as_deref(),
        Some("Scoped probe")
    );
    assert!(scope.capabilities().supports_history_compaction);
    assert_eq!(scope.list_models().await.expect("list models").len(), 1);

    let streamed = collect_response_from_stream(scope.stream(request()).await.expect("stream"))
        .await
        .expect("collect stream");
    assert_eq!(response_text(&streamed), "generation=0 turn=1");

    let sent = scope.send(request()).await.expect("send override");
    assert_eq!(response_text(&sent), "send generation=0");
    assert_eq!(
        scope
            .compact(compaction_request())
            .await
            .expect("compact override"),
        CompactionResponse::from_text("compact generation=0")
    );
    assert_eq!(
        scope
            .summarize_memories(memory_request())
            .await
            .expect("summarize override")
            .output[0]
            .raw_memory,
        "raw generation=0"
    );
    let _fresh = scope.fresh_session_scope().expect("fresh override");

    assert_eq!(calls.list_models.load(Ordering::SeqCst), 1);
    assert_eq!(calls.stream.load(Ordering::SeqCst), 1);
    assert_eq!(calls.send.load(Ordering::SeqCst), 1);
    assert_eq!(calls.compact.load(Ordering::SeqCst), 1);
    assert_eq!(calls.summarize.load(Ordering::SeqCst), 1);
    assert_eq!(calls.fresh.load(Ordering::SeqCst), 1);
}
