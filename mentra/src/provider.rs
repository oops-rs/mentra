use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

pub use mentra_provider::AnthropicRequestOptions;
pub use mentra_provider::AuthScheme;
pub use mentra_provider::BuiltinProvider;
pub use mentra_provider::CompactionInputItem;
pub use mentra_provider::CompactionRequest;
pub use mentra_provider::CompactionResponse;
pub use mentra_provider::ContentBlock;
pub use mentra_provider::ContentBlockDelta;
pub use mentra_provider::ContentBlockStart;
pub use mentra_provider::EmbeddingData;
pub use mentra_provider::EmbeddingModelInfo;
pub use mentra_provider::EmbeddingProvider;
pub use mentra_provider::EmbeddingRequest;
pub use mentra_provider::EmbeddingResponse;
pub use mentra_provider::EmbeddingUsage;
pub use mentra_provider::GeminiRequestOptions;
pub use mentra_provider::ImageSource;
pub use mentra_provider::MemorySummarizeOutput;
pub use mentra_provider::MemorySummarizeRequest;
pub use mentra_provider::MemorySummarizeResponse;
pub use mentra_provider::Message;
pub use mentra_provider::ModelInfo;
pub use mentra_provider::ModelSelector;
pub use mentra_provider::OpenAIRequestOptions;
pub use mentra_provider::ProviderCapabilities;
pub use mentra_provider::ProviderCredentials;
pub use mentra_provider::ProviderDefinition;
pub use mentra_provider::ProviderDescriptor;
pub use mentra_provider::ProviderError;
pub use mentra_provider::ProviderEvent;
pub use mentra_provider::ProviderEventStream;
pub use mentra_provider::ProviderId;
pub use mentra_provider::ProviderRequestOptions;
pub use mentra_provider::RawMemory;
pub use mentra_provider::RawMemoryMetadata;
pub use mentra_provider::ReasoningEffort;
pub use mentra_provider::ReasoningFormat;
pub use mentra_provider::ReasoningOptions;
pub use mentra_provider::ReasoningProvenance;
pub use mentra_provider::Request;
pub use mentra_provider::Response;
pub use mentra_provider::ResponsesRequestOptions;
pub use mentra_provider::ResponsesStateMode;
pub use mentra_provider::ResponsesTransport;
pub use mentra_provider::RetryPolicy;
pub use mentra_provider::Role;
pub use mentra_provider::TokenUsage;
pub use mentra_provider::ToolChoice;
pub use mentra_provider::ToolSearchMode;
pub use mentra_provider::WireApi;
pub use mentra_provider::collect_response_from_stream;
pub use mentra_provider::provider_event_stream_from_response;

pub mod model {
    pub use mentra_provider::AnthropicRequestOptions;
    pub use mentra_provider::ContentBlock;
    pub use mentra_provider::ContentBlockDelta;
    pub use mentra_provider::ContentBlockStart;
    pub use mentra_provider::ImageSource;
    pub use mentra_provider::MemorySummarizeOutput;
    pub use mentra_provider::MemorySummarizeRequest;
    pub use mentra_provider::MemorySummarizeResponse;
    pub use mentra_provider::Message;
    pub use mentra_provider::ModelInfo;
    pub use mentra_provider::OpenAIRequestOptions;
    pub use mentra_provider::ProviderError;
    pub use mentra_provider::ProviderEvent;
    pub use mentra_provider::ProviderEventStream;
    pub use mentra_provider::ProviderId;
    pub use mentra_provider::ProviderRequestOptions;
    pub use mentra_provider::RawMemory;
    pub use mentra_provider::RawMemoryMetadata;
    pub use mentra_provider::ReasoningEffort;
    pub use mentra_provider::ReasoningFormat;
    pub use mentra_provider::ReasoningOptions;
    pub use mentra_provider::ReasoningProvenance;
    pub use mentra_provider::Request;
    pub use mentra_provider::Response;
    pub use mentra_provider::ResponsesStateMode;
    pub use mentra_provider::ResponsesTransport;
    pub use mentra_provider::Role;
    pub use mentra_provider::TokenUsage;
    pub use mentra_provider::ToolChoice;
    pub use mentra_provider::ToolSearchMode;
    pub use mentra_provider::collect_response_from_stream;
    pub use mentra_provider::provider_event_stream_from_response;
}

/// Transport-neutral interface implemented by model providers.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Returns identifying metadata for the provider instance.
    fn descriptor(&self) -> ProviderDescriptor;

    /// Returns feature flags supported by this provider instance.
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }

    /// Lists models available from the provider.
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError>;

    /// Streams a model response for the given request.
    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError>;

    /// Sends a request and collects the full response in memory.
    async fn send(&self, request: Request<'_>) -> Result<Response, ProviderError> {
        collect_response_from_stream(self.stream(request).await?).await
    }

    /// Compacts transcript history using a provider-native endpoint when supported.
    async fn compact(
        &self,
        _request: CompactionRequest<'_>,
    ) -> Result<CompactionResponse, ProviderError> {
        Err(ProviderError::UnsupportedCapability(
            "history_compaction".to_string(),
        ))
    }

    /// Summarizes raw trace memories using a provider-native implementation when supported.
    async fn summarize_memories(
        &self,
        _request: MemorySummarizeRequest<'_>,
    ) -> Result<MemorySummarizeResponse, ProviderError> {
        Err(ProviderError::UnsupportedCapability(
            "memory_summarization".to_string(),
        ))
    }
}

#[derive(Default)]
pub struct ProviderRegistry {
    default_provider: Option<ProviderId>,
    default_embedding_provider: Option<ProviderId>,
    providers: HashMap<ProviderId, Arc<dyn Provider>>,
    embedding_providers: HashMap<ProviderId, Arc<dyn EmbeddingProvider>>,
    /// The Responses transport this runtime's requests go out on, or `None`
    /// when the runtime does not choose and each request's own options stand.
    ///
    /// It lives here rather than on the handle because a transport is a
    /// property of the connection to a provider, and this is where a runtime
    /// keeps those. It also means the choice travels the one path the builder
    /// already hands to the handle at build time, instead of a field every
    /// `with_*` reconstructor would have to remember to carry.
    responses_transport: Option<ResponsesTransport>,
}

impl ProviderRegistry {
    pub(crate) fn register_builtin_provider(
        &mut self,
        id: BuiltinProvider,
        api_key: impl Into<String>,
    ) -> Result<(), String> {
        let api_key = api_key.into();
        let provider: Arc<dyn Provider> = match id {
            BuiltinProvider::Anthropic => {
                Arc::new(anthropic::AnthropicProvider::new(api_key.clone()))
            }
            BuiltinProvider::Gemini => Arc::new(gemini::GeminiProvider::new(api_key.clone())),
            BuiltinProvider::OpenAI => Arc::new(openai::OpenAIProvider::new(api_key.clone())),
            BuiltinProvider::OpenRouter => {
                Arc::new(openrouter::OpenRouterProvider::new(api_key.clone()))
            }
            BuiltinProvider::Ollama => Arc::new(ollama::OllamaProvider::new()),
            BuiltinProvider::LmStudio => Arc::new(lmstudio::LmStudioProvider::new()),
        };

        let provider_id: ProviderId = id.into();

        if self.default_provider.is_none() {
            self.default_provider = Some(provider_id.clone());
        }

        // Register embedding provider for providers that support it.
        let ep: Option<Arc<dyn EmbeddingProvider>> = match id {
            BuiltinProvider::OpenAI => Some(Arc::new(mentra_provider::responses::openai(api_key))),
            BuiltinProvider::OpenRouter => {
                Some(Arc::new(mentra_provider::responses::openrouter(api_key)))
            }
            BuiltinProvider::Ollama => Some(Arc::new(openai_compatible_embedding_provider(
                id,
                "http://127.0.0.1:11434/",
            ))),
            BuiltinProvider::LmStudio => Some(Arc::new(openai_compatible_embedding_provider(
                id,
                "http://127.0.0.1:1234/",
            ))),
            _ => None,
        };
        if let Some(ep) = ep {
            if self.default_embedding_provider.is_none() {
                self.default_embedding_provider = Some(provider_id.clone());
            }
            self.embedding_providers.insert(provider_id.clone(), ep);
        }

        self.providers.insert(provider_id, provider);
        Ok(())
    }

    pub(crate) fn register_provider_instance<P>(&mut self, provider: P)
    where
        P: Provider + 'static,
    {
        let descriptor = provider.descriptor();
        let id = descriptor.id;

        if self.default_provider.is_none() {
            self.default_provider = Some(id.clone());
        }

        self.providers.insert(id, Arc::new(provider));
    }

    pub(crate) fn register_registered_provider<P>(&mut self, provider: P)
    where
        P: mentra_provider::Provider + 'static,
    {
        let descriptor = provider.descriptor();
        let id = descriptor.id;

        if self.default_provider.is_none() {
            self.default_provider = Some(id.clone());
        }

        self.providers.insert(id, shared_provider(provider));
    }

    pub(crate) fn register_ollama(&mut self) {
        self.register_provider_instance(ollama::OllamaProvider::new());
    }

    pub(crate) fn register_lmstudio(&mut self) {
        self.register_provider_instance(lmstudio::LmStudioProvider::new());
    }

    pub(crate) fn get_provider(&self, id: Option<&ProviderId>) -> Option<Arc<dyn Provider>> {
        match id {
            Some(id) => self.providers.get(id).cloned(),
            None => self
                .default_provider
                .as_ref()
                .and_then(|id| self.providers.get(id).cloned()),
        }
    }

    /// Returns the default embedding provider, or `None` if no embedding-capable provider
    /// has been registered.
    ///
    /// The default is the first embedding-capable provider registered. To look up a
    /// specific provider use [`embedding_provider_for`].
    pub fn embedding_provider(&self) -> Option<Arc<dyn EmbeddingProvider>> {
        self.default_embedding_provider
            .as_ref()
            .and_then(|id| self.embedding_providers.get(id))
            .map(Arc::clone)
            .or_else(|| self.embedding_providers.values().next().map(Arc::clone))
    }

    /// Returns the embedding provider for a specific provider ID, or `None`.
    pub fn embedding_provider_for(&self, id: &ProviderId) -> Option<Arc<dyn EmbeddingProvider>> {
        self.embedding_providers.get(id).map(Arc::clone)
    }

    pub(crate) fn descriptors(&self) -> Vec<ProviderDescriptor> {
        self.providers
            .values()
            .map(|provider| provider.descriptor())
            .collect()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    pub(crate) fn set_responses_transport(&mut self, transport: ResponsesTransport) {
        self.responses_transport = Some(transport);
    }

    pub(crate) fn responses_transport(&self) -> Option<ResponsesTransport> {
        self.responses_transport
    }
}

/// Settles which Responses transport a request goes out on, and refuses one the
/// provider cannot serve.
///
/// The runtime's choice, when it made one, replaces whatever the request's own
/// options carried: it is the connection-level answer, and a per-request one
/// that disagreed would mean two live opinions about a single socket. With no
/// runtime choice the request's own value stands, which is what every caller
/// had before a runtime could choose at all.
///
/// A provider whose capabilities report no websocket support is refused rather
/// than quietly served over HTTP+SSE. The fallback is the tempting behavior and
/// the wrong one: asking for a transport is explicit, so answering on a
/// different one returns a stream nobody asked for and hides a misconfigured
/// runtime behind a working one — the same stance `stream_response` already
/// takes when the transport is not compiled in.
pub(crate) fn select_responses_transport(
    provider: &dyn Provider,
    chosen: Option<ResponsesTransport>,
    options: &mut ProviderRequestOptions,
) -> Result<(), crate::error::RuntimeError> {
    if let Some(transport) = chosen {
        options.responses.transport = transport;
    }

    if options.responses.transport != ResponsesTransport::WebSocket
        || provider.capabilities().supports_websockets
    {
        return Ok(());
    }

    let descriptor = provider.descriptor();
    let name = descriptor
        .display_name
        .unwrap_or_else(|| descriptor.id.as_str().to_string());
    Err(crate::error::RuntimeError::OperationDenied(format!(
        "provider '{name}' does not serve the Responses websocket transport; \
         select ResponsesTransport::HttpSse or register a provider that does \
         — answering over HTTP+SSE would return a transport nobody asked for"
    )))
}

fn shared_provider<P>(provider: P) -> Arc<dyn Provider>
where
    P: mentra_provider::Provider + 'static,
{
    Arc::new(SharedProviderProxy { inner: provider })
}

/// Builds a `ResponsesProvider` (with no credentials) for OpenAI-compatible
/// local providers (Ollama, LmStudio) so they can be used as embedding providers.
fn openai_compatible_embedding_provider(
    builtin: BuiltinProvider,
    base_url: &str,
) -> mentra_provider::responses::ResponsesProvider<NoCredentialsSource> {
    use mentra_provider::AuthScheme;
    use mentra_provider::ProviderCapabilities;
    use mentra_provider::RetryPolicy;
    use mentra_provider::WireApi;
    use std::collections::HashMap;

    let mut definition = ProviderDefinition::new(builtin);
    definition.wire_api = WireApi::Responses;
    definition.auth_scheme = AuthScheme::None;
    definition.capabilities = ProviderCapabilities {
        supports_model_listing: true,
        supports_streaming: true,
        supports_websockets: false,
        supports_tool_calls: true,
        supports_images: true,
        supports_history_compaction: false,
        supports_memory_summarization: false,
        supports_deferred_tools: false,
        supports_hosted_tool_search: false,
        supports_hosted_web_search: false,
        supports_image_generation: false,
        supports_reasoning_effort: false,
        reports_reasoning_tokens: false,
        reports_thoughts_tokens: false,
        supports_structured_tool_results: false,
        supports_embeddings: true,
    };
    definition.base_url = Some(base_url.to_string());
    definition.headers = Some(HashMap::new());
    definition.retry = RetryPolicy::default();
    mentra_provider::responses::ResponsesProvider::new(definition, NoCredentialsSource)
}

/// Builds a provider for an endpoint speaking the OpenAI `chat/completions`
/// wire.
///
/// This is the wire the OpenAI-compatible ecosystem actually implements.
/// `v1/responses` is OpenAI's own, and an endpoint that does not serve it
/// answers 404 to every turn — with an error that reads like a mistyped base
/// URL rather than like a wire mismatch.
fn chat_completions_provider(
    provider: impl Into<mentra_provider::ProviderId>,
    display_name: &str,
    description: &str,
    base_url: &str,
    credentials: Option<String>,
) -> Arc<dyn Provider> {
    let mut definition = mentra_provider::chat_completions::definition(provider, base_url);
    definition.descriptor.display_name = Some(display_name.to_string());
    definition.descriptor.description = Some(description.to_string());

    match credentials {
        Some(api_key) => shared_provider(
            mentra_provider::chat_completions::ChatCompletionsProvider::new(
                definition,
                mentra_provider::StaticCredentialSource::new(api_key),
            ),
        ),
        None => {
            definition.auth_scheme = mentra_provider::AuthScheme::None;
            shared_provider(
                mentra_provider::chat_completions::ChatCompletionsProvider::new(
                    definition,
                    NoCredentialsSource,
                ),
            )
        }
    }
}

#[derive(Clone)]
struct NoCredentialsSource;

#[async_trait]
impl mentra_provider::CredentialSource for NoCredentialsSource {
    async fn credentials(
        &self,
    ) -> Result<mentra_provider::ProviderCredentials, mentra_provider::ProviderError> {
        Ok(mentra_provider::ProviderCredentials::default())
    }
}

struct SharedProviderProxy<P> {
    inner: P,
}

#[async_trait]
impl<P> Provider for SharedProviderProxy<P>
where
    P: mentra_provider::Provider + 'static,
{
    fn descriptor(&self) -> ProviderDescriptor {
        self.inner.descriptor()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.definition().capabilities
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.inner.list_models().await
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        self.inner.stream(request).await
    }

    async fn compact(
        &self,
        request: CompactionRequest<'_>,
    ) -> Result<CompactionResponse, ProviderError> {
        self.inner.compact(request).await
    }

    async fn summarize_memories(
        &self,
        request: MemorySummarizeRequest<'_>,
    ) -> Result<MemorySummarizeResponse, ProviderError> {
        self.inner.summarize_memories(request).await
    }
}

pub mod openai {
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::CompactionRequest;
    use super::CompactionResponse;
    use super::Provider;
    use super::ProviderCapabilities;
    use super::ProviderDescriptor;
    use super::ProviderError;
    use super::ProviderEventStream;
    use super::Request;
    use super::shared_provider;

    use crate::provider::model::ModelInfo;

    /// Supplies OpenAI API credentials on demand.
    #[async_trait]
    pub trait OpenAICredentialSource: Send + Sync {
        async fn api_key(&self) -> Result<String, String>;
    }

    #[derive(Clone)]
    pub struct OpenAIProvider {
        inner: Arc<dyn Provider>,
    }

    impl OpenAIProvider {
        pub fn new(api_key: impl Into<String>) -> Self {
            Self {
                inner: shared_provider(mentra_provider::responses::openai(api_key)),
            }
        }

        pub fn with_credential_source(source: impl OpenAICredentialSource + 'static) -> Self {
            Self::with_shared_credential_source(Arc::new(source))
        }

        pub fn with_shared_credential_source(source: Arc<dyn OpenAICredentialSource>) -> Self {
            let provider = mentra_provider::responses::openai_with_credential_source(
                OpenAICredentialAdapter { source },
            );
            Self {
                inner: shared_provider(provider),
            }
        }
    }

    #[async_trait]
    impl Provider for OpenAIProvider {
        fn descriptor(&self) -> ProviderDescriptor {
            self.inner.descriptor()
        }

        fn capabilities(&self) -> ProviderCapabilities {
            self.inner.capabilities()
        }

        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            self.inner.list_models().await
        }

        async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
            self.inner.stream(request).await
        }
        async fn compact(
            &self,
            request: CompactionRequest<'_>,
        ) -> Result<CompactionResponse, ProviderError> {
            self.inner.compact(request).await
        }

        async fn summarize_memories(
            &self,
            request: super::MemorySummarizeRequest<'_>,
        ) -> Result<super::MemorySummarizeResponse, ProviderError> {
            self.inner.summarize_memories(request).await
        }
    }

    #[derive(Clone)]
    struct OpenAICredentialAdapter {
        source: Arc<dyn OpenAICredentialSource>,
    }

    #[async_trait]
    impl mentra_provider::CredentialSource for OpenAICredentialAdapter {
        async fn credentials(
            &self,
        ) -> Result<mentra_provider::ProviderCredentials, mentra_provider::ProviderError> {
            let api_key = self
                .source
                .api_key()
                .await
                .map_err(mentra_provider::ProviderError::InvalidRequest)?;

            Ok(mentra_provider::ProviderCredentials {
                bearer_token: Some(api_key),
                account_id: None,
                headers: Default::default(),
            })
        }
    }
}

pub mod openrouter {
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::CompactionRequest;
    use super::CompactionResponse;
    use super::Provider;
    use super::ProviderCapabilities;
    use super::ProviderDescriptor;
    use super::ProviderError;
    use super::ProviderEventStream;
    use super::Request;
    use super::shared_provider;
    use crate::provider::model::ModelInfo;

    #[derive(Clone)]
    pub struct OpenRouterProvider {
        inner: Arc<dyn Provider>,
    }

    impl OpenRouterProvider {
        pub fn new(api_key: impl Into<String>) -> Self {
            Self {
                inner: shared_provider(mentra_provider::responses::openrouter(api_key)),
            }
        }
    }

    #[async_trait]
    impl Provider for OpenRouterProvider {
        fn descriptor(&self) -> ProviderDescriptor {
            self.inner.descriptor()
        }

        fn capabilities(&self) -> ProviderCapabilities {
            self.inner.capabilities()
        }

        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            self.inner.list_models().await
        }

        async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
            self.inner.stream(request).await
        }
        async fn compact(
            &self,
            request: CompactionRequest<'_>,
        ) -> Result<CompactionResponse, ProviderError> {
            self.inner.compact(request).await
        }

        async fn summarize_memories(
            &self,
            request: super::MemorySummarizeRequest<'_>,
        ) -> Result<super::MemorySummarizeResponse, ProviderError> {
            self.inner.summarize_memories(request).await
        }
    }
}

pub mod anthropic {
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::Provider;
    use super::ProviderDescriptor;
    use super::ProviderError;
    use super::ProviderEventStream;
    use super::Request;
    use super::shared_provider;
    use crate::provider::model::ModelInfo;

    #[derive(Clone)]
    pub struct AnthropicProvider {
        inner: Arc<dyn Provider>,
    }

    impl AnthropicProvider {
        pub fn new(api_key: impl Into<String>) -> Self {
            Self {
                inner: shared_provider(mentra_provider::anthropic::AnthropicProvider::new(api_key)),
            }
        }
    }

    #[async_trait]
    impl Provider for AnthropicProvider {
        fn descriptor(&self) -> ProviderDescriptor {
            self.inner.descriptor()
        }

        fn capabilities(&self) -> super::ProviderCapabilities {
            self.inner.capabilities()
        }

        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            self.inner.list_models().await
        }

        async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
            self.inner.stream(request).await
        }

        async fn summarize_memories(
            &self,
            request: super::MemorySummarizeRequest<'_>,
        ) -> Result<super::MemorySummarizeResponse, ProviderError> {
            self.inner.summarize_memories(request).await
        }
    }
}

pub mod gemini {
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::Provider;
    use super::ProviderDescriptor;
    use super::ProviderError;
    use super::ProviderEventStream;
    use super::Request;
    use super::shared_provider;
    use crate::provider::model::ModelInfo;

    #[derive(Clone)]
    pub struct GeminiProvider {
        inner: Arc<dyn Provider>,
    }

    impl GeminiProvider {
        pub fn new(api_key: impl Into<String>) -> Self {
            Self {
                inner: shared_provider(mentra_provider::gemini::GeminiProvider::new(api_key)),
            }
        }
    }

    #[async_trait]
    impl Provider for GeminiProvider {
        fn descriptor(&self) -> ProviderDescriptor {
            self.inner.descriptor()
        }

        fn capabilities(&self) -> super::ProviderCapabilities {
            self.inner.capabilities()
        }

        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            self.inner.list_models().await
        }

        async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
            self.inner.stream(request).await
        }

        async fn summarize_memories(
            &self,
            request: super::MemorySummarizeRequest<'_>,
        ) -> Result<super::MemorySummarizeResponse, ProviderError> {
            self.inner.summarize_memories(request).await
        }
    }
}

/// Providers for any endpoint speaking the OpenAI `chat/completions` wire.
///
/// DeepSeek, Groq, Together, Fireworks, Mistral, xAI, OpenRouter, vLLM,
/// llama.cpp, Ollama and LM Studio all serve this wire; almost none of them
/// serve `v1/responses`. Point this at a base URL and it will speak the thing
/// on the other end.
pub mod openai_compatible {
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::MemorySummarizeRequest;
    use super::MemorySummarizeResponse;
    use super::Provider;
    use super::ProviderCapabilities;
    use super::ProviderDescriptor;
    use super::ProviderError;
    use super::ProviderEventStream;
    use super::Request;
    use crate::provider::model::ModelInfo;

    /// A provider for one OpenAI-compatible endpoint.
    ///
    /// ```rust,no_run
    /// use mentra::provider::openai_compatible::OpenAiCompatibleProvider;
    ///
    /// let deepseek = OpenAiCompatibleProvider::new(
    ///     "deepseek",
    ///     "https://api.deepseek.com/",
    ///     std::env::var("DEEPSEEK_API_KEY").unwrap(),
    /// );
    /// ```
    #[derive(Clone)]
    pub struct OpenAiCompatibleProvider {
        inner: Arc<dyn Provider>,
    }

    impl OpenAiCompatibleProvider {
        /// Registers an endpoint that authenticates with a bearer token.
        ///
        /// `id` is the name the runtime will know this provider by, and can be
        /// anything not already registered.
        pub fn new(
            id: impl Into<mentra_provider::ProviderId>,
            base_url: impl AsRef<str>,
            api_key: impl Into<String>,
        ) -> Self {
            let id = id.into();
            let display_name = id.as_str().to_string();
            Self {
                inner: super::chat_completions_provider(
                    id,
                    &display_name,
                    "OpenAI-compatible chat/completions provider",
                    base_url.as_ref(),
                    Some(api_key.into()),
                ),
            }
        }

        /// Registers an endpoint that wants no credentials — a local vLLM or
        /// llama.cpp server, say.
        pub fn without_credentials(
            id: impl Into<mentra_provider::ProviderId>,
            base_url: impl AsRef<str>,
        ) -> Self {
            let id = id.into();
            let display_name = id.as_str().to_string();
            Self {
                inner: super::chat_completions_provider(
                    id,
                    &display_name,
                    "OpenAI-compatible chat/completions provider",
                    base_url.as_ref(),
                    None,
                ),
            }
        }
    }

    #[async_trait]
    impl Provider for OpenAiCompatibleProvider {
        fn descriptor(&self) -> ProviderDescriptor {
            self.inner.descriptor()
        }

        fn capabilities(&self) -> ProviderCapabilities {
            self.inner.capabilities()
        }

        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            self.inner.list_models().await
        }

        async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
            self.inner.stream(request).await
        }

        async fn summarize_memories(
            &self,
            request: MemorySummarizeRequest<'_>,
        ) -> Result<MemorySummarizeResponse, ProviderError> {
            self.inner.summarize_memories(request).await
        }
    }
}

pub mod ollama {
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::BuiltinProvider;
    use super::Provider;
    use super::ProviderDescriptor;
    use super::ProviderError;
    use super::ProviderEventStream;
    use super::Request;
    use crate::provider::model::ModelInfo;

    const DEFAULT_BASE_URL: &str = "http://127.0.0.1:11434/";

    #[derive(Clone)]
    pub struct OllamaProvider {
        inner: Arc<dyn Provider>,
    }

    impl OllamaProvider {
        pub fn new() -> Self {
            Self::with_base_url(DEFAULT_BASE_URL)
        }

        pub fn with_base_url(base_url: impl AsRef<str>) -> Self {
            Self {
                // Ollama serves `v1/chat/completions` and has never served
                // `v1/responses`.
                inner: super::chat_completions_provider(
                    BuiltinProvider::Ollama,
                    "Ollama",
                    "Ollama OpenAI-compatible chat/completions provider",
                    base_url.as_ref(),
                    None,
                ),
            }
        }
    }

    impl Default for OllamaProvider {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl Provider for OllamaProvider {
        fn descriptor(&self) -> ProviderDescriptor {
            self.inner.descriptor()
        }

        fn capabilities(&self) -> super::ProviderCapabilities {
            self.inner.capabilities()
        }

        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            self.inner.list_models().await
        }

        async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
            self.inner.stream(request).await
        }

        async fn summarize_memories(
            &self,
            request: super::MemorySummarizeRequest<'_>,
        ) -> Result<super::MemorySummarizeResponse, ProviderError> {
            self.inner.summarize_memories(request).await
        }
    }
}

pub mod lmstudio {
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::BuiltinProvider;
    use super::Provider;
    use super::ProviderDescriptor;
    use super::ProviderError;
    use super::ProviderEventStream;
    use super::Request;
    use crate::provider::model::ModelInfo;

    const DEFAULT_BASE_URL: &str = "http://127.0.0.1:1234/";

    #[derive(Clone)]
    pub struct LmStudioProvider {
        inner: Arc<dyn Provider>,
    }

    impl LmStudioProvider {
        pub fn new() -> Self {
            Self::with_base_url(DEFAULT_BASE_URL)
        }

        pub fn with_base_url(base_url: impl AsRef<str>) -> Self {
            Self {
                // LM Studio's OpenAI-compatible surface is
                // `v1/chat/completions`; only recent builds serve
                // `v1/responses` at all.
                inner: super::chat_completions_provider(
                    BuiltinProvider::LmStudio,
                    "LM Studio",
                    "LM Studio OpenAI-compatible chat/completions provider",
                    base_url.as_ref(),
                    None,
                ),
            }
        }
    }

    impl Default for LmStudioProvider {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl Provider for LmStudioProvider {
        fn descriptor(&self) -> ProviderDescriptor {
            self.inner.descriptor()
        }

        fn capabilities(&self) -> super::ProviderCapabilities {
            self.inner.capabilities()
        }

        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            self.inner.list_models().await
        }

        async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
            self.inner.stream(request).await
        }

        async fn summarize_memories(
            &self,
            request: super::MemorySummarizeRequest<'_>,
        ) -> Result<super::MemorySummarizeResponse, ProviderError> {
            self.inner.summarize_memories(request).await
        }
    }
}
