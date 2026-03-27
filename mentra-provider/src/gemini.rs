use async_trait::async_trait;
use std::sync::Arc;

pub(crate) mod model;
pub(crate) mod sse;

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

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/";

pub struct GeminiProvider<C = StaticCredentialSource> {
    client: reqwest::Client,
    credential_source: Arc<C>,
    definition: ProviderDefinition,
}

impl<C> Clone for GeminiProvider<C> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            credential_source: Arc::clone(&self.credential_source),
            definition: self.definition.clone(),
        }
    }
}

impl GeminiProvider<StaticCredentialSource> {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_credential_source(StaticCredentialSource::new(api_key))
    }
}

impl<C> GeminiProvider<C>
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
        let mut definition = ProviderDefinition::new(BuiltinProvider::Gemini);
        definition.descriptor.display_name = Some("Gemini".to_string());
        definition.descriptor.description =
            Some("Google Gemini Developer API provider".to_string());
        definition.wire_api = WireApi::GeminiGenerateContent;
        definition.auth_scheme = AuthScheme::Header {
            name: "x-goog-api-key".to_string(),
        };
        definition.capabilities = ProviderCapabilities {
            supports_model_listing: true,
            supports_streaming: true,
            supports_websockets: false,
            supports_tool_calls: true,
            supports_images: true,
            supports_history_compaction: true,
            supports_memory_summarization: true,
            supports_deferred_tools: false,
            supports_hosted_tool_search: false,
            supports_hosted_web_search: false,
            supports_image_generation: false,
            supports_reasoning_effort: true,
            reports_reasoning_tokens: false,
            reports_thoughts_tokens: true,
            supports_structured_tool_results: false,
        };
        definition.base_url = Some(DEFAULT_BASE_URL.to_string());
        definition
    }
}

#[async_trait]
impl<C> ModelCatalog for GeminiProvider<C>
where
    C: CredentialSource + 'static,
{
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let mut models = Vec::new();
        let mut page_token = None::<String>;

        loop {
            let credentials = self.credential_source.credentials().await?;
            let mut request = self
                .client
                .get(
                    self.definition
                        .request_url_with_auth_for_path("v1beta/models", &credentials)?,
                )
                .headers(self.definition.build_headers(&credentials)?)
                .query(&[("pageSize", "1000")]);

            if let Some(token) = page_token.as_deref() {
                request = request.query(&[("pageToken", token)]);
            }

            let response = request.send().await.map_err(ProviderError::Transport)?;
            if !response.status().is_success() {
                return Err(ProviderError::Http {
                    status: response.status(),
                    body: response.text().await.unwrap_or_default(),
                });
            }

            let page = response
                .json::<model::GeminiModelsPage>()
                .await
                .map_err(ProviderError::Decode)?;

            models.extend(
                page.models
                    .into_iter()
                    .filter(|model| model.supports_generate_content())
                    .map(ModelInfo::from),
            );

            page_token = page.next_page_token;
            if page_token.is_none() {
                break;
            }
        }

        Ok(models)
    }
}

#[async_trait]
impl<C> ProviderSessionFactory for GeminiProvider<C>
where
    C: CredentialSource + 'static,
{
    async fn create_session(&self) -> Result<Box<dyn ProviderSession>, ProviderError> {
        Ok(Box::new((*self).clone()))
    }
}

#[async_trait]
impl<C> ProviderSession for GeminiProvider<C>
where
    C: CredentialSource + 'static,
{
    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let session = request.provider_request_options.session.clone();
        let model_name = request.model.to_string();
        let request = model::GeminiGenerateContentRequest::try_from(request)?;
        let credentials = self.credential_source.credentials().await?;
        let response = self
            .client
            .post(self.definition.request_url_with_auth_for_path(
                &format!(
                    "v1beta/{}:streamGenerateContent",
                    normalize_model_name(&model_name)
                ),
                &credentials,
            )?)
            .headers(self.definition.build_headers_for_session(
                &credentials,
                Some(&session),
                None,
            )?)
            .query(&[("alt", "sse")])
            .json(&request)
            .send()
            .await
            .map_err(ProviderError::Transport)?;

        if !response.status().is_success() {
            return Err(ProviderError::Http {
                status: response.status(),
                body: response.text().await.unwrap_or_default(),
            });
        }

        Ok(sse::spawn_event_stream(response, model_name))
    }

    async fn compact(
        &self,
        request: CompactionRequest<'_>,
    ) -> Result<CompactionResponse, ProviderError> {
        let request = request.into_model_request()?;
        let response = ProviderSession::send(self, request).await?;
        Ok(response.into_compaction_response())
    }

    async fn summarize_memories(
        &self,
        request: crate::MemorySummarizeRequest<'_>,
    ) -> Result<crate::MemorySummarizeResponse, ProviderError> {
        let request = request.into_model_request()?;
        let response = ProviderSession::send(self, request).await?;
        response.into_memory_summarize_response()
    }
}

#[async_trait]
impl<C> RegisteredProvider for GeminiProvider<C>
where
    C: CredentialSource + 'static,
{
    fn definition(&self) -> ProviderDefinition {
        self.definition.clone()
    }
}

fn normalize_model_name(model: &str) -> String {
    if model.starts_with("models/") {
        model.to_string()
    } else {
        format!("models/{model}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RegisteredProvider;

    #[test]
    fn definition_advertises_history_compaction_support() {
        let provider = GeminiProvider::new("test-key");

        assert!(
            provider
                .definition()
                .capabilities
                .supports_history_compaction
        );
    }
}
