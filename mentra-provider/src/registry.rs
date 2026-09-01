use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

use crate::definition::ProviderDefinition;
use crate::definition::ProviderDescriptor;
use crate::definition::ProviderId;
use crate::error::ProviderError;
use crate::model::ModelInfo;
use crate::request::CompactionRequest;
use crate::request::MemorySummarizeRequest;
use crate::request::Request;
use crate::response::CompactionResponse;
use crate::response::MemorySummarizeResponse;
use crate::response::Response;
use crate::response::collect_response_from_stream;
use crate::stream::ProviderEventStream;

/// Lists models available from a provider.
#[async_trait]
pub trait ModelCatalog: Send + Sync {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError>;
}

/// Creates a provider session on demand.
#[async_trait]
pub trait ProviderSessionFactory: Send + Sync {
    async fn create_session(&self) -> Result<Box<dyn ProviderSession>, ProviderError>;
}

/// Transport-neutral session used to stream model responses.
#[async_trait]
pub trait ProviderSession: Send + Sync {
    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError>;

    async fn send(&self, request: Request<'_>) -> Result<Response, ProviderError> {
        collect_response_from_stream(self.stream(request).await?).await
    }

    async fn compact(
        &self,
        _request: CompactionRequest<'_>,
    ) -> Result<CompactionResponse, ProviderError> {
        Err(ProviderError::UnsupportedCapability(
            "history_compaction".to_string(),
        ))
    }

    async fn summarize_memories(
        &self,
        _request: MemorySummarizeRequest<'_>,
    ) -> Result<MemorySummarizeResponse, ProviderError> {
        Err(ProviderError::UnsupportedCapability(
            "memory_summarization".to_string(),
        ))
    }
}

/// Transport-neutral provider registration interface.
#[async_trait]
pub trait Provider: ModelCatalog + ProviderSessionFactory {
    fn definition(&self) -> ProviderDefinition;

    fn descriptor(&self) -> ProviderDescriptor {
        self.definition().descriptor
    }

    /// Returns the same configured provider with an independent session scope.
    ///
    /// Creating a scope is a synchronous, local operation: it must not open a
    /// connection or otherwise perform network I/O. Ordinary clones of the
    /// returned value share that one scope; calling this method again creates a
    /// different one. Implementations must preserve their provider definition,
    /// including its descriptor identity.
    ///
    /// Providers that do not define how to separate configuration from session
    /// state remain usable for one-shot runtimes and report this capability as
    /// unsupported.
    fn fresh_session_scope(&self) -> Result<ProviderSessionScope, ProviderError> {
        Err(ProviderError::UnsupportedCapability(
            "fresh_session_scope".to_string(),
        ))
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        self.create_session().await?.stream(request).await
    }

    async fn send(&self, request: Request<'_>) -> Result<Response, ProviderError> {
        collect_response_from_stream(self.stream(request).await?).await
    }

    async fn compact(
        &self,
        request: CompactionRequest<'_>,
    ) -> Result<CompactionResponse, ProviderError> {
        self.create_session().await?.compact(request).await
    }

    async fn summarize_memories(
        &self,
        request: MemorySummarizeRequest<'_>,
    ) -> Result<MemorySummarizeResponse, ProviderError> {
        self.create_session()
            .await?
            .summarize_memories(request)
            .await
    }
}

/// One provider bound to one provider-owned session-state scope.
///
/// Cloning this value deliberately shares the current scope. Use
/// [`Provider::fresh_session_scope`] to retain the provider configuration while
/// allocating independent turn, response-chain, connection, and in-flight
/// state.
#[derive(Clone)]
pub struct ProviderSessionScope {
    inner: Arc<dyn Provider>,
}

impl ProviderSessionScope {
    /// Wraps a provider whose current session state is the scope to share.
    pub fn new<P>(provider: P) -> Self
    where
        P: Provider + 'static,
    {
        Self {
            inner: Arc::new(provider),
        }
    }
}

#[async_trait]
impl ModelCatalog for ProviderSessionScope {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.inner.list_models().await
    }
}

#[async_trait]
impl ProviderSessionFactory for ProviderSessionScope {
    async fn create_session(&self) -> Result<Box<dyn ProviderSession>, ProviderError> {
        self.inner.create_session().await
    }
}

#[async_trait]
impl Provider for ProviderSessionScope {
    fn definition(&self) -> ProviderDefinition {
        self.inner.definition()
    }

    fn descriptor(&self) -> ProviderDescriptor {
        self.inner.descriptor()
    }

    fn fresh_session_scope(&self) -> Result<ProviderSessionScope, ProviderError> {
        self.inner.fresh_session_scope()
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        self.inner.stream(request).await
    }

    async fn send(&self, request: Request<'_>) -> Result<Response, ProviderError> {
        self.inner.send(request).await
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

pub use Provider as RegisteredProvider;

#[derive(Default)]
pub struct ProviderRegistry {
    default_provider: Option<ProviderId>,
    providers: HashMap<ProviderId, Arc<dyn Provider>>,
}

impl ProviderRegistry {
    pub fn register_provider_instance<P>(&mut self, provider: P)
    where
        P: Provider + 'static,
    {
        let definition = provider.definition();
        let id = definition.descriptor.id.clone();

        if self.default_provider.is_none() {
            self.default_provider = Some(id.clone());
        }

        self.providers.insert(id, Arc::new(provider));
    }

    pub fn get_provider(&self, id: Option<&ProviderId>) -> Option<Arc<dyn Provider>> {
        match id {
            Some(id) => self.providers.get(id).cloned(),
            None => self
                .default_provider
                .as_ref()
                .and_then(|id| self.providers.get(id).cloned()),
        }
    }

    pub fn definitions(&self) -> Vec<ProviderDefinition> {
        self.providers
            .values()
            .map(|provider| provider.definition())
            .collect()
    }

    pub fn descriptors(&self) -> Vec<ProviderDescriptor> {
        self.providers
            .values()
            .map(|provider| provider.descriptor())
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;

    #[derive(Clone)]
    struct TestProvider {
        definition: ProviderDefinition,
        models: Vec<ModelInfo>,
    }

    struct TestSession;

    #[derive(Clone)]
    struct RefreshableProvider {
        definition: ProviderDefinition,
        models: Vec<ModelInfo>,
        fresh_scopes: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ModelCatalog for TestProvider {
        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(self.models.clone())
        }
    }

    #[async_trait]
    impl ProviderSessionFactory for TestProvider {
        async fn create_session(&self) -> Result<Box<dyn ProviderSession>, ProviderError> {
            Ok(Box::new(TestSession))
        }
    }

    #[async_trait]
    impl Provider for TestProvider {
        fn definition(&self) -> ProviderDefinition {
            self.definition.clone()
        }
    }

    #[async_trait]
    impl ModelCatalog for RefreshableProvider {
        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(self.models.clone())
        }
    }

    #[async_trait]
    impl ProviderSessionFactory for RefreshableProvider {
        async fn create_session(&self) -> Result<Box<dyn ProviderSession>, ProviderError> {
            Ok(Box::new(TestSession))
        }
    }

    #[async_trait]
    impl Provider for RefreshableProvider {
        fn definition(&self) -> ProviderDefinition {
            self.definition.clone()
        }

        fn descriptor(&self) -> ProviderDescriptor {
            let mut descriptor = self.definition.descriptor.clone();
            descriptor.description = Some("delegated descriptor".to_string());
            descriptor
        }

        fn fresh_session_scope(&self) -> Result<ProviderSessionScope, ProviderError> {
            self.fresh_scopes.fetch_add(1, Ordering::SeqCst);
            Ok(ProviderSessionScope::new(self.clone()))
        }
    }

    #[async_trait]
    impl ProviderSession for TestSession {
        async fn stream(
            &self,
            _request: Request<'_>,
        ) -> Result<ProviderEventStream, ProviderError> {
            let (_tx, rx) = mpsc::unbounded_channel();
            Ok(rx)
        }
    }

    #[tokio::test]
    async fn registry_returns_registered_provider_descriptors() {
        let mut registry = ProviderRegistry::default();
        let provider = TestProvider {
            definition: ProviderDefinition::new("test-provider"),
            models: vec![ModelInfo::new("model-1", "test-provider")],
        };

        registry.register_provider_instance(provider);

        assert_eq!(registry.descriptors().len(), 1);
        assert_eq!(registry.definitions().len(), 1);
        assert_eq!(
            registry
                .get_provider(None)
                .expect("provider should exist")
                .definition()
                .descriptor
                .id
                .as_str(),
            "test-provider"
        );
    }

    #[test]
    fn an_arbitrary_provider_is_not_reusable_without_an_explicit_scope_contract() {
        let provider = TestProvider {
            definition: ProviderDefinition::new("one-shot"),
            models: Vec::new(),
        };
        let provider: &dyn Provider = &provider;

        let error = provider
            .fresh_session_scope()
            .err()
            .expect("a provider cannot claim freshness by default");

        assert!(matches!(
            error,
            ProviderError::UnsupportedCapability(capability)
                if capability == "fresh_session_scope"
        ));
    }

    #[tokio::test]
    async fn a_scope_clone_shares_its_provider_while_freshening_delegates_repeatedly() {
        let model = ModelInfo::new("model-1", "refreshable");
        let fresh_scopes = Arc::new(AtomicUsize::new(0));
        let scope = ProviderSessionScope::new(RefreshableProvider {
            definition: ProviderDefinition::new("refreshable"),
            models: vec![model.clone()],
            fresh_scopes: Arc::clone(&fresh_scopes),
        });
        let shared = scope.clone();

        assert!(Arc::ptr_eq(&scope.inner, &shared.inner));

        let first = scope
            .fresh_session_scope()
            .expect("the provider mints its first independent scope");
        let second = first
            .fresh_session_scope()
            .expect("the wrapper delegates repeated freshening");

        assert!(!Arc::ptr_eq(&scope.inner, &first.inner));
        assert!(!Arc::ptr_eq(&first.inner, &second.inner));
        assert_eq!(fresh_scopes.load(Ordering::SeqCst), 2);
        assert_eq!(first.definition(), scope.definition());
        assert_eq!(second.definition(), scope.definition());
        assert_eq!(
            second.descriptor().description.as_deref(),
            Some("delegated descriptor")
        );
        assert_eq!(
            second.list_models().await.expect("delegated catalog"),
            vec![model]
        );
        let _ = second
            .create_session()
            .await
            .expect("delegated session factory");
    }

    #[test]
    fn each_concrete_provider_preserves_its_definition_in_a_fresh_scope() {
        let mut anthropic_definition = crate::anthropic::definition();
        anthropic_definition.descriptor.id = ProviderId::new("custom-anthropic");
        anthropic_definition.base_url = Some("https://anthropic.example/v1/".to_string());

        let chat_definition =
            crate::chat_completions::definition("custom-chat", "https://chat.example/v1/");

        let mut gemini_definition = ProviderDefinition::new("custom-gemini");
        gemini_definition.base_url = Some("https://gemini.example/v1/".to_string());

        let mut responses_definition = crate::responses::openai_definition();
        responses_definition.descriptor.id = ProviderId::new("custom-responses");
        responses_definition.base_url = Some("https://responses.example/v1/".to_string());

        let providers: Vec<Box<dyn Provider>> = vec![
            Box::new(
                crate::anthropic::AnthropicProvider::with_definition_and_credential_source(
                    anthropic_definition,
                    crate::StaticCredentialSource::new("anthropic-key"),
                ),
            ),
            Box::new(crate::chat_completions::ChatCompletionsProvider::new(
                chat_definition,
                crate::StaticCredentialSource::new("chat-key"),
            )),
            Box::new(
                crate::gemini::GeminiProvider::with_definition_and_credential_source(
                    gemini_definition,
                    crate::StaticCredentialSource::new("gemini-key"),
                ),
            ),
            Box::new(crate::responses::ResponsesProvider::new(
                responses_definition,
                crate::StaticCredentialSource::new("responses-key"),
            )),
        ];

        for provider in providers {
            let expected = provider.definition();
            let fresh = provider
                .fresh_session_scope()
                .expect("each concrete provider supports fresh scopes");
            assert_eq!(fresh.definition(), expected);
        }
    }
}
