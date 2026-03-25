use async_trait::async_trait;
use std::{collections::HashMap, sync::Arc};

use crate::{
    definition::{ProviderDefinition, ProviderDescriptor, ProviderId},
    error::ProviderError,
    model::ModelInfo,
    request::{CompactionRequest, Request},
    response::{CompactionResponse, Response, collect_response_from_stream},
    stream::ProviderEventStream,
};

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
}

/// Transport-neutral provider registration interface.
#[async_trait]
pub trait Provider: ModelCatalog + ProviderSessionFactory {
    fn definition(&self) -> ProviderDefinition;

    fn descriptor(&self) -> ProviderDescriptor {
        self.definition().descriptor
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
    use tokio::sync::mpsc;

    #[derive(Clone)]
    struct TestProvider {
        definition: ProviderDefinition,
        models: Vec<ModelInfo>,
    }

    struct TestSession;

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
}
