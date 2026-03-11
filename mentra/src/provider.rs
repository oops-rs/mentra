use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;

use crate::provider::{
    anthropic::AnthropicProvider,
    model::{
        ModelInfo, ModelProviderKind, ProviderError, ProviderEventStream, Request, Response,
        collect_response_from_stream,
    },
};

pub mod anthropic;
pub mod model;

#[async_trait]
pub trait Provider: Send + Sync {
    fn kind(&self) -> ModelProviderKind;

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError>;

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError>;

    async fn send(&self, request: Request<'_>) -> Result<Response, ProviderError> {
        collect_response_from_stream(self.stream(request).await?).await
    }
}

#[derive(Default)]
pub struct ProviderRegistry {
    default_provider: Option<ModelProviderKind>,
    providers: HashMap<ModelProviderKind, Arc<dyn Provider>>,
}

impl ProviderRegistry {
    pub fn register_provider(&mut self, kind: ModelProviderKind, api_key: impl Into<String>) {
        let provider = match kind {
            ModelProviderKind::Anthropic => Arc::new(AnthropicProvider::new(api_key)),
            _ => todo!("Add support for new provider"),
        };

        if self.default_provider.is_none() {
            self.default_provider = Some(kind);
        }

        self.providers.insert(kind, provider);
    }

    pub fn register_provider_instance<P>(&mut self, provider: P)
    where
        P: Provider + 'static,
    {
        let kind = provider.kind();

        if self.default_provider.is_none() {
            self.default_provider = Some(kind);
        }

        self.providers.insert(kind, Arc::new(provider));
    }

    pub fn get_provider(&self, kind: Option<ModelProviderKind>) -> Option<Arc<dyn Provider>> {
        kind.and_then(|kind| self.providers.get(&kind).cloned())
            .or_else(|| {
                self.default_provider
                    .and_then(|kind| self.providers.get(&kind).cloned())
            })
    }

    pub fn get_default_provider(&self) -> Option<Arc<dyn Provider>> {
        self.default_provider
            .and_then(|kind| self.providers.get(&kind).cloned())
    }
}
