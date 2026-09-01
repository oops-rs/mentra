pub mod model;
pub mod session;
pub mod sse;
/// The `response.create` websocket transport. Compiled in with the
/// `responses-websocket` feature; without it, a request that selects
/// [`ResponsesTransport::WebSocket`](crate::ResponsesTransport::WebSocket)
/// fails rather than falling back to HTTP.
#[cfg(feature = "responses-websocket")]
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
use crate::ProviderSessionScope;
use crate::RegisteredProvider;
use crate::RetryPolicy;
use crate::StaticCredentialSource;
use crate::WireApi;
use crate::embedding::EmbeddingModelInfo;
use crate::embedding::EmbeddingProvider;
use crate::embedding::EmbeddingRequest;
use crate::embedding::EmbeddingResponse;

use self::session::ResponsesEndpointCapabilities;
use self::session::ResponsesSession;
use self::session::ResponsesSessionState;

pub(crate) type SharedTurnState = Arc<std::sync::Mutex<Option<String>>>;

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
    endpoint_capabilities: Arc<ResponsesEndpointCapabilities>,
    hybrid_http_previous_response_id: bool,
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
        // The idle timeout, not a total one: a streamed turn can legitimately
        // run for minutes, but a gap between chunks means the provider stopped
        // talking. `read_timeout` resets on every successful read, so it bounds
        // the silence without bounding the turn. The resulting error is a
        // `Transport` error, which the runtime already treats as transient and
        // retries.
        let client = reqwest::Client::builder()
            .read_timeout(definition.stream_idle_timeout)
            .build()
            .expect("failed to build reqwest client");
        Self {
            definition,
            credential_source,
            client,
            session_state: Arc::new(ResponsesSessionState::default()),
            endpoint_capabilities: Arc::new(ResponsesEndpointCapabilities::default()),
            hybrid_http_previous_response_id: true,
        }
    }

    pub fn definition(&self) -> &ProviderDefinition {
        &self.definition
    }

    /// Disables opportunistic `previous_response_id` chaining for Hybrid HTTP
    /// requests made by this provider.
    ///
    /// Use this when the endpoint is already known not to accept that optional
    /// Responses parameter. Hybrid requests retain their complete local replay,
    /// so disabling the optimization avoids a known-failing discovery request.
    /// Stateful requests, explicit response ids, and WebSocket transport keep
    /// their existing behavior.
    pub fn without_hybrid_http_previous_response_id(mut self) -> Self {
        self.hybrid_http_previous_response_id = false;
        self
    }

    /// Returns the same configured provider with an independent session scope.
    ///
    /// The provider definition, credential source, HTTP client and its connection pool, and
    /// endpoint capability knowledge are retained. Response chaining, turn affinity, WebSocket
    /// connections, and in-flight session state start empty. Clone this returned value when a
    /// runtime and its prewarm handle must share the newly allocated scope; ordinary [`Clone`]
    /// continues to share the current scope.
    pub fn fresh_session_scope(&self) -> Self {
        Self {
            definition: self.definition.clone(),
            credential_source: Arc::clone(&self.credential_source),
            client: self.client.clone(),
            session_state: Arc::new(ResponsesSessionState::default()),
            endpoint_capabilities: Arc::clone(&self.endpoint_capabilities),
            hybrid_http_previous_response_id: self.hybrid_http_previous_response_id,
        }
    }

    pub fn session(&self) -> ResponsesSession<C> {
        ResponsesSession::new(
            self.definition.clone(),
            Arc::clone(&self.credential_source),
            self.client.clone(),
            Arc::clone(&self.session_state),
            Arc::clone(&self.endpoint_capabilities),
            self.hybrid_http_previous_response_id,
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
        supports_embeddings: true,
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
            return Err(ProviderError::from_http_response(response).await);
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

    fn fresh_session_scope(&self) -> Result<ProviderSessionScope, ProviderError> {
        Ok(ProviderSessionScope::new(
            ResponsesProvider::fresh_session_scope(self),
        ))
    }
}

#[async_trait]
impl<C> EmbeddingProvider for ResponsesProvider<C>
where
    C: CredentialSource + 'static,
{
    async fn embed_batch(
        &self,
        model: &str,
        texts: &[&str],
    ) -> Result<EmbeddingResponse, ProviderError> {
        let credentials = self.credential_source.credentials().await?;
        let url = self
            .definition
            .request_url_with_auth_for_path("v1/embeddings", &credentials)?;
        let headers = self.definition.build_headers(&credentials)?;
        let body = EmbeddingRequest::batch(model, texts);

        let response = self
            .client
            .post(url)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .map_err(ProviderError::Transport)?;

        if !response.status().is_success() {
            return Err(ProviderError::from_http_response(response).await);
        }

        response
            .json::<EmbeddingResponse>()
            .await
            .map_err(ProviderError::Decode)
    }

    fn embedding_models(&self) -> Vec<EmbeddingModelInfo> {
        // Available embedding models depend on the specific provider instance and its
        // configuration. Callers should use the /v1/models endpoint for discovery
        // rather than relying on a static list that would only be accurate for OpenAI.
        vec![]
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

    #[test]
    fn disabling_hybrid_http_state_on_a_clone_does_not_reconfigure_the_original() {
        let original = openai("test-key");
        let disabled = original.clone().without_hybrid_http_previous_response_id();

        assert!(original.hybrid_http_previous_response_id);
        assert!(!disabled.hybrid_http_previous_response_id);
    }

    #[test]
    fn clone_keeps_the_existing_shared_session_scope() {
        let provider = openai("test-key");
        let clone = provider.clone();

        assert!(Arc::ptr_eq(&provider.session_state, &clone.session_state));
        assert!(Arc::ptr_eq(
            &provider.endpoint_capabilities,
            &clone.endpoint_capabilities
        ));
        assert!(Arc::ptr_eq(
            &provider.credential_source,
            &clone.credential_source
        ));
    }

    #[test]
    fn fresh_scope_preserves_provider_configuration_and_splits_only_session_state() {
        let mut definition = openai_definition();
        definition.descriptor.id = ProviderId::new("custom-responses");
        definition.base_url = Some("https://example.test/custom/".to_string());
        definition
            .headers
            .get_or_insert_default()
            .insert("x-provider-config".to_string(), "preserved".to_string());
        let provider = ResponsesProvider::with_shared_credential_source(
            definition.clone(),
            Arc::new(StaticCredentialSource::new("test-key")),
        )
        .without_hybrid_http_previous_response_id();

        let fresh = provider.fresh_session_scope();

        assert_eq!(fresh.definition, definition);
        assert!(Arc::ptr_eq(
            &provider.credential_source,
            &fresh.credential_source
        ));
        assert!(!Arc::ptr_eq(&provider.session_state, &fresh.session_state));
        assert!(Arc::ptr_eq(
            &provider.endpoint_capabilities,
            &fresh.endpoint_capabilities
        ));
        assert!(!fresh.hybrid_http_previous_response_id);
    }
}
