use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;

pub use mentra_provider::{
    AnthropicRequestOptions, AuthScheme, BuiltinProvider, CompactionInputItem, CompactionRequest,
    CompactionResponse, ContentBlock, ContentBlockDelta, ContentBlockStart, GeminiRequestOptions,
    ImageSource, Message, ModelInfo, ModelSelector, OpenAIRequestOptions, ProviderCapabilities,
    ProviderCredentials, ProviderDefinition, ProviderDescriptor, ProviderError, ProviderEvent,
    ProviderEventStream, ProviderId, ProviderRequestOptions, ReasoningEffort, ReasoningOptions,
    Request, Response, ResponsesRequestOptions, RetryPolicy, Role, TokenUsage, ToolChoice,
    ToolSearchMode, WireApi, collect_response_from_stream, provider_event_stream_from_response,
};

pub mod model {
    pub use mentra_provider::{
        AnthropicRequestOptions, ContentBlock, ContentBlockDelta, ContentBlockStart, ImageSource,
        Message, ModelInfo, OpenAIRequestOptions, ProviderError, ProviderEvent,
        ProviderEventStream, ProviderId, ProviderRequestOptions, ReasoningEffort, ReasoningOptions,
        Request, Response, Role, TokenUsage, ToolChoice, ToolSearchMode,
        collect_response_from_stream, provider_event_stream_from_response,
    };
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
}

#[derive(Default)]
pub struct ProviderRegistry {
    default_provider: Option<ProviderId>,
    providers: HashMap<ProviderId, Arc<dyn Provider>>,
}

impl ProviderRegistry {
    pub(crate) fn register_builtin_provider(
        &mut self,
        id: BuiltinProvider,
        api_key: impl Into<String>,
    ) -> Result<(), String> {
        let provider: Arc<dyn Provider> = match id {
            BuiltinProvider::Anthropic => Arc::new(anthropic::AnthropicProvider::new(api_key)),
            BuiltinProvider::Gemini => Arc::new(gemini::GeminiProvider::new(api_key)),
            BuiltinProvider::OpenAI => Arc::new(openai::OpenAIProvider::new(api_key)),
            BuiltinProvider::OpenRouter => Arc::new(openrouter::OpenRouterProvider::new(api_key)),
            BuiltinProvider::Ollama => Arc::new(ollama::OllamaProvider::new()),
            BuiltinProvider::LmStudio => Arc::new(lmstudio::LmStudioProvider::new()),
        };

        let id: ProviderId = id.into();

        if self.default_provider.is_none() {
            self.default_provider = Some(id.clone());
        }

        self.providers.insert(id, provider);
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

    pub(crate) fn descriptors(&self) -> Vec<ProviderDescriptor> {
        self.providers
            .values()
            .map(|provider| provider.descriptor())
            .collect()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

fn shared_provider<P>(provider: P) -> Arc<dyn Provider>
where
    P: mentra_provider::Provider + 'static,
{
    Arc::new(SharedProviderProxy { inner: provider })
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
}

pub mod openai {
    use std::{collections::HashMap, sync::Arc};

    use async_trait::async_trait;

    use super::{
        AuthScheme, BuiltinProvider, CompactionRequest, CompactionResponse, Provider,
        ProviderCapabilities, ProviderDefinition, ProviderDescriptor, ProviderError,
        ProviderEventStream, Request, RetryPolicy, WireApi, shared_provider,
    };

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

        pub(crate) fn openai_compatible(
            provider: BuiltinProvider,
            display_name: &'static str,
            description: &'static str,
            base_url: &str,
        ) -> Self {
            let mut definition = ProviderDefinition::new(provider);
            definition.descriptor.display_name = Some(display_name.to_string());
            definition.descriptor.description = Some(description.to_string());
            definition.wire_api = WireApi::Responses;
            definition.auth_scheme = AuthScheme::None;
            definition.capabilities = ProviderCapabilities {
                supports_model_listing: true,
                supports_streaming: true,
                supports_websockets: false,
                supports_tool_calls: true,
                supports_images: true,
                supports_history_compaction: false,
                supports_deferred_tools: false,
                supports_hosted_tool_search: false,
                supports_hosted_web_search: false,
                supports_image_generation: false,
                supports_reasoning_effort: false,
                reports_reasoning_tokens: false,
                reports_thoughts_tokens: false,
                supports_structured_tool_results: false,
            };
            definition.base_url = Some(base_url.to_string());
            definition.headers = Some(HashMap::new());
            definition.retry = RetryPolicy::default();

            let provider =
                mentra_provider::responses::ResponsesProvider::new(definition, NoCredentialsSource);
            Self {
                inner: shared_provider(provider),
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
    }

    #[derive(Clone)]
    struct OpenAICredentialAdapter {
        source: Arc<dyn OpenAICredentialSource>,
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

    use super::{
        CompactionRequest, CompactionResponse, Provider, ProviderCapabilities, ProviderDescriptor,
        ProviderError, ProviderEventStream, Request, shared_provider,
    };
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
    }
}

pub mod anthropic {
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::{
        Provider, ProviderDescriptor, ProviderError, ProviderEventStream, Request, shared_provider,
    };
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
    }
}

pub mod gemini {
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::{
        Provider, ProviderDescriptor, ProviderError, ProviderEventStream, Request, shared_provider,
    };
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
    }
}

pub mod ollama {
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::{
        BuiltinProvider, Provider, ProviderDescriptor, ProviderError, ProviderEventStream, Request,
    };
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
                inner: Arc::new(super::openai::OpenAIProvider::openai_compatible(
                    BuiltinProvider::Ollama,
                    "Ollama",
                    "Ollama OpenAI-compatible Responses API provider",
                    base_url.as_ref(),
                )),
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
    }
}

pub mod lmstudio {
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::{
        BuiltinProvider, Provider, ProviderDescriptor, ProviderError, ProviderEventStream, Request,
    };
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
                inner: Arc::new(super::openai::OpenAIProvider::openai_compatible(
                    BuiltinProvider::LmStudio,
                    "LM Studio",
                    "LM Studio OpenAI-compatible Responses API provider",
                    base_url.as_ref(),
                )),
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
    }
}
