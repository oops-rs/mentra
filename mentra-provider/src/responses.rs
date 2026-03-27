pub mod model;
pub mod session;
pub mod sse;
pub mod websocket;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::AuthScheme;
use crate::BuiltinProvider;
use crate::CredentialSource;
use crate::ModelCatalog;
use crate::ModelInfo;
use crate::ProviderCapabilities;
use crate::ProviderDefinition;
use crate::ProviderError;
use crate::ProviderSessionFactory;
use crate::RegisteredProvider;
use crate::RetryPolicy;
use crate::StaticCredentialSource;
use crate::WireApi;

use self::session::ResponsesSession;
use self::session::ResponsesSessionState;

const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/";
const DEFAULT_OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/";

pub fn openai(api_key: impl Into<String>) -> ResponsesProvider<StaticCredentialSource> {
    ResponsesProvider::openai(api_key)
}

pub fn openrouter(api_key: impl Into<String>) -> ResponsesProvider<StaticCredentialSource> {
    ResponsesProvider::openrouter(api_key)
}

pub fn openai_with_credential_source<C>(credential_source: C) -> ResponsesProvider<C>
where
    C: CredentialSource + 'static,
{
    ResponsesProvider::openai_with_credential_source(credential_source)
}

pub fn openrouter_with_credential_source<C>(credential_source: C) -> ResponsesProvider<C>
where
    C: CredentialSource + 'static,
{
    ResponsesProvider::openrouter_with_credential_source(credential_source)
}

/// Shared Responses-family provider implementation.
///
/// This type owns the provider definition, credential source, client, and transport state while
/// the request mapping and SSE decoding live in the sibling modules.
#[derive(Clone)]
pub struct ResponsesProvider<C> {
    definition: ProviderDefinition,
    credential_source: Arc<C>,
    client: reqwest::Client,
    session_state: Arc<ResponsesSessionState>,
}

impl<C> ResponsesProvider<C>
where
    C: CredentialSource + 'static,
{
    pub fn new(definition: ProviderDefinition, credential_source: C) -> Self {
        Self::with_shared_credential_source(definition, Arc::new(credential_source))
    }

    pub fn with_shared_credential_source(
        definition: ProviderDefinition,
        credential_source: Arc<C>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .build()
            .expect("failed to build reqwest client");
        Self {
            definition,
            credential_source,
            client,
            session_state: Arc::new(ResponsesSessionState::default()),
        }
    }

    pub fn definition(&self) -> &ProviderDefinition {
        &self.definition
    }

    pub fn session(&self) -> ResponsesSession<C> {
        ResponsesSession::new(
            self.definition.clone(),
            Arc::clone(&self.credential_source),
            self.client.clone(),
            Arc::clone(&self.session_state),
        )
    }

    pub fn openai_with_credential_source(credential_source: C) -> Self {
        Self::with_shared_credential_source(openai_definition(), Arc::new(credential_source))
    }

    pub fn openrouter_with_credential_source(credential_source: C) -> Self {
        Self::with_shared_credential_source(openrouter_definition(), Arc::new(credential_source))
    }
}

impl ResponsesProvider<StaticCredentialSource> {
    pub fn openai(api_key: impl Into<String>) -> Self {
        Self::openai_with_credential_source(StaticCredentialSource::new(api_key))
    }

    pub fn openrouter(api_key: impl Into<String>) -> Self {
        Self::openrouter_with_credential_source(StaticCredentialSource::new(api_key))
    }
}

pub fn openai_definition() -> ProviderDefinition {
    build_definition(
        BuiltinProvider::OpenAI,
        "OpenAI",
        "OpenAI Responses API provider",
        DEFAULT_OPENAI_BASE_URL,
    )
}

pub fn openrouter_definition() -> ProviderDefinition {
    build_definition(
        BuiltinProvider::OpenRouter,
        "OpenRouter",
        "OpenRouter Responses API provider",
        DEFAULT_OPENROUTER_BASE_URL,
    )
}

fn build_definition(
    builtin: BuiltinProvider,
    display_name: &str,
    description: &str,
    base_url: &str,
) -> ProviderDefinition {
    let mut definition = ProviderDefinition::new(builtin);
    definition.descriptor.display_name = Some(display_name.to_string());
    definition.descriptor.description = Some(description.to_string());
    definition.wire_api = WireApi::Responses;
    definition.auth_scheme = AuthScheme::BearerToken;
    definition.capabilities = ProviderCapabilities {
        supports_model_listing: true,
        supports_streaming: true,
        supports_websockets: true,
        supports_tool_calls: true,
        supports_images: true,
        supports_history_compaction: true,
        supports_memory_summarization: true,
        supports_deferred_tools: true,
        supports_hosted_tool_search: true,
        supports_hosted_web_search: true,
        supports_image_generation: true,
        supports_reasoning_effort: true,
        reports_reasoning_tokens: true,
        reports_thoughts_tokens: false,
        supports_structured_tool_results: true,
    };
    definition.base_url = Some(base_url.to_string());
    definition.headers = Some(HashMap::new());
    definition.retry = RetryPolicy::default();
    definition
}

#[async_trait]
impl<C> ModelCatalog for ResponsesProvider<C>
where
    C: CredentialSource + 'static,
{
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let credentials = self.credential_source.credentials().await?;
        let request = self
            .client
            .get(
                self.definition
                    .request_url_with_auth_for_path("v1/models", &credentials)?,
            )
            .headers(self.definition.build_headers(&credentials)?);

        let response = request.send().await.map_err(ProviderError::Transport)?;

        if !response.status().is_success() {
            return Err(ProviderError::Http {
                status: response.status(),
                body: response.text().await.unwrap_or_default(),
            });
        }

        let models = response
            .json::<self::model::ResponsesModelsPage>()
            .await
            .map_err(ProviderError::Decode)?;

        Ok(models.into_model_info(self.definition.descriptor.id.clone()))
    }
}

#[async_trait]
impl<C> ProviderSessionFactory for ResponsesProvider<C>
where
    C: CredentialSource + 'static,
{
    async fn create_session(&self) -> Result<Box<dyn crate::ProviderSession>, ProviderError> {
        Ok(Box::new(self.session()))
    }
}

#[async_trait]
impl<C> RegisteredProvider for ResponsesProvider<C>
where
    C: CredentialSource + 'static,
{
    fn definition(&self) -> ProviderDefinition {
        self.definition.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderId;

    #[test]
    fn openai_preset_uses_responses_wire_api() {
        let provider = openai("test-key");
        let definition = provider.definition();

        assert_eq!(
            definition.descriptor.id,
            ProviderId::from(BuiltinProvider::OpenAI)
        );
        assert_eq!(
            definition.descriptor.display_name.as_deref(),
            Some("OpenAI")
        );
        assert_eq!(definition.wire_api, WireApi::Responses);
        assert!(definition.capabilities.supports_websockets);
        assert!(definition.capabilities.supports_history_compaction);
        assert_eq!(
            definition.base_url.as_deref(),
            Some(DEFAULT_OPENAI_BASE_URL)
        );
    }

    #[test]
    fn openrouter_preset_uses_openrouter_base_url() {
        let provider = openrouter("test-key");
        let definition = provider.definition();

        assert_eq!(
            definition.descriptor.id,
            ProviderId::from(BuiltinProvider::OpenRouter)
        );
        assert_eq!(
            definition.descriptor.display_name.as_deref(),
            Some("OpenRouter")
        );
        assert_eq!(definition.wire_api, WireApi::Responses);
        assert!(definition.capabilities.supports_history_compaction);
        assert_eq!(
            definition.base_url.as_deref(),
            Some(DEFAULT_OPENROUTER_BASE_URL)
        );
    }
}
