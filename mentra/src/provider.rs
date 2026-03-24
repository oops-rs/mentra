use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;

pub mod anthropic;
pub mod gemini;
mod model;
pub mod openai;
pub mod openrouter;

use self::{
    anthropic::AnthropicProvider, gemini::GeminiProvider, openai::OpenAIProvider,
    openrouter::OpenRouterProvider,
};

pub use model::{
    AnthropicRequestOptions, BuiltinProvider, ContentBlock, ContentBlockDelta, ContentBlockStart,
    ImageSource, Message, ModelInfo, ModelSelector, OpenAIRequestOptions, ProviderDescriptor,
    ProviderError, ProviderEvent, ProviderEventStream, ProviderId, ProviderRequestOptions,
    ReasoningEffort, ReasoningOptions, Request, Response, Role, TokenUsage, ToolChoice,
    ToolSearchMode, collect_response_from_stream, provider_event_stream_from_response,
};

/// Transport-neutral interface implemented by model providers.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Returns identifying metadata for the provider instance.
    fn descriptor(&self) -> ProviderDescriptor;

    /// Lists models available from the provider.
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError>;

    /// Streams a model response for the given request.
    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError>;

    /// Sends a request and collects the full response in memory.
    async fn send(&self, request: Request<'_>) -> Result<Response, ProviderError> {
        collect_response_from_stream(self.stream(request).await?).await
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
            BuiltinProvider::Anthropic => Arc::new(AnthropicProvider::new(api_key)),
            BuiltinProvider::Gemini => Arc::new(GeminiProvider::new(api_key)),
            BuiltinProvider::OpenAI => Arc::new(OpenAIProvider::new(api_key)),
            BuiltinProvider::OpenRouter => Arc::new(OpenRouterProvider::new(api_key)),
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
