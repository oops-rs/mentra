use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) mod model;
pub(crate) mod sse;
pub(crate) mod stream_model;

use crate::AuthScheme;
use crate::BuiltinProvider;
use crate::CompactionRequest;
use crate::CompactionResponse;
use crate::CredentialSource;
use crate::ModelCatalog;
use crate::ModelInfo;
use crate::ProviderCapabilities;
use crate::ProviderDefinition;
use crate::ProviderError;
use crate::ProviderEventStream;
use crate::ProviderSession;
use crate::ProviderSessionFactory;
use crate::RegisteredProvider;
use crate::Request;
use crate::StaticCredentialSource;
use crate::WireApi;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider<C = StaticCredentialSource> {
    client: reqwest::Client,
    credential_source: Arc<C>,
    definition: ProviderDefinition,
}

impl<C> Clone for AnthropicProvider<C> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            credential_source: Arc::clone(&self.credential_source),
            definition: self.definition.clone(),
        }
    }
}

impl AnthropicProvider<StaticCredentialSource> {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_credential_source(StaticCredentialSource::new(api_key))
    }
}

impl<C> AnthropicProvider<C>
where
    C: CredentialSource + 'static,
{
    pub fn with_credential_source(credential_source: C) -> Self {
        Self::with_shared_credential_source(Arc::new(credential_source))
    }

    pub fn with_shared_credential_source(credential_source: Arc<C>) -> Self {
        Self::with_definition_and_shared_credential_source(Self::definition(), credential_source)
    }

    pub fn with_definition_and_credential_source(
        definition: ProviderDefinition,
        credential_source: C,
    ) -> Self {
        Self::with_definition_and_shared_credential_source(definition, Arc::new(credential_source))
    }

    pub fn with_definition_and_shared_credential_source(
        definition: ProviderDefinition,
        credential_source: Arc<C>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .build()
            .expect("Failed to build client");

        Self {
            client,
            credential_source,
            definition,
        }
    }

    fn definition() -> ProviderDefinition {
        let mut definition = ProviderDefinition::new(BuiltinProvider::Anthropic);
        definition.descriptor.display_name = Some("Anthropic".to_string());
        definition.descriptor.description = Some("Anthropic Messages API provider".to_string());
        definition.wire_api = WireApi::AnthropicMessages;
        definition.auth_scheme = AuthScheme::Header {
            name: "x-api-key".to_string(),
        };
        definition.capabilities = ProviderCapabilities {
            supports_model_listing: true,
            supports_streaming: true,
            supports_websockets: false,
            supports_tool_calls: true,
            supports_images: true,
            supports_history_compaction: true,
            supports_deferred_tools: true,
            supports_hosted_tool_search: true,
            supports_hosted_web_search: false,
            supports_image_generation: false,
            supports_reasoning_effort: true,
            reports_reasoning_tokens: false,
            reports_thoughts_tokens: false,
            supports_structured_tool_results: false,
        };
        definition.base_url = Some(DEFAULT_BASE_URL.to_string());
        definition.headers = Some(HashMap::from([(
            "anthropic-version".to_string(),
            ANTHROPIC_VERSION.to_string(),
        )]));
        definition
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RegisteredProvider;

    #[test]
    fn definition_advertises_history_compaction_support() {
        let provider = AnthropicProvider::new("test-key");

        assert!(
            provider
                .definition()
                .capabilities
                .supports_history_compaction
        );
    }
}

#[async_trait]
impl<C> ModelCatalog for AnthropicProvider<C>
where
    C: CredentialSource + 'static,
{
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let mut models = Vec::new();
        let mut after_id = None;

        loop {
            let credentials = self.credential_source.credentials().await?;
            let request = self
                .client
                .get(
                    self.definition
                        .request_url_with_auth_for_path("v1/models", &credentials)?,
                )
                .headers(self.definition.build_headers(&credentials)?)
                .query(&[
                    ("limit", "1000"),
                    ("after_id", after_id.as_deref().unwrap_or("")),
                ]);

            let response = request.send().await.map_err(ProviderError::Transport)?;

            if !response.status().is_success() {
                return Err(ProviderError::Http {
                    status: response.status(),
                    body: response.text().await.unwrap_or_default(),
                });
            }

            let page = response
                .json::<model::AnthropicModelsPage>()
                .await
                .map_err(ProviderError::Decode)?;

            after_id = page.last_id.clone();
            models.extend(page.data.into_iter().map(|model| model.into()));

            if !page.has_more {
                break;
            }
        }

        Ok(models)
    }
}

#[async_trait]
impl<C> ProviderSessionFactory for AnthropicProvider<C>
where
    C: CredentialSource + 'static,
{
    async fn create_session(&self) -> Result<Box<dyn ProviderSession>, ProviderError> {
        Ok(Box::new((*self).clone()))
    }
}

#[async_trait]
impl<C> ProviderSession for AnthropicProvider<C>
where
    C: CredentialSource + 'static,
{
    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let response = self.send_message(request, true).await?;
        Ok(sse::spawn_event_stream(response))
    }

    async fn compact(
        &self,
        request: CompactionRequest<'_>,
    ) -> Result<CompactionResponse, ProviderError> {
        let request = request.into_model_request()?;
        let response = ProviderSession::send(self, request).await?;
        Ok(response.into_compaction_response())
    }
}

#[async_trait]
impl<C> RegisteredProvider for AnthropicProvider<C>
where
    C: CredentialSource + 'static,
{
    fn definition(&self) -> ProviderDefinition {
        self.definition.clone()
    }
}

impl<C> AnthropicProvider<C>
where
    C: CredentialSource + 'static,
{
    async fn send_message(
        &self,
        request: Request<'_>,
        stream: bool,
    ) -> Result<reqwest::Response, ProviderError> {
        let session = request.provider_request_options.session.clone();
        let request = model::AnthropicRequest::try_from(request)?;
        let mut body = serde_json::to_value(request).map_err(ProviderError::Serialize)?;
        if stream {
            body["stream"] = Value::Bool(true);
        }
        let credentials = self.credential_source.credentials().await?;
        let response = self
            .client
            .post(
                self.definition
                    .request_url_with_auth_for_path("v1/messages", &credentials)?,
            )
            .headers(self.definition.build_headers_for_session(
                &credentials,
                Some(&session),
                None,
            )?)
            .json(&body)
            .send()
            .await
            .map_err(ProviderError::Transport)?;

        if !response.status().is_success() {
            return Err(ProviderError::Http {
                status: response.status(),
                body: response.text().await.unwrap_or_default(),
            });
        }

        Ok(response)
    }
}
