//! A client for TypeSafe's System One API.
//!
//! System One is an independently invoked evaluation API; it is not an agent
//! model provider and does not automatically participate in run budgets.

mod model;

use std::sync::Arc;

pub use model::*;

use crate::{
    AuthScheme, CredentialSource, ProviderDefinition, ProviderError, StaticCredentialSource,
};

/// TypeSafe's documented alias for its current general-purpose Jev model.
pub const DEFAULT_JEV_MODEL: &str = "jev-latest";

/// A client for TypeSafe's `systemone` and model-list endpoints.
pub struct SystemOneClient<C> {
    definition: ProviderDefinition,
    credential_source: Arc<C>,
    client: reqwest::Client,
    images_enabled: bool,
    long_context_enabled: bool,
}

impl<C> Clone for SystemOneClient<C> {
    fn clone(&self) -> Self {
        Self {
            definition: self.definition.clone(),
            credential_source: Arc::clone(&self.credential_source),
            client: self.client.clone(),
            images_enabled: self.images_enabled,
            long_context_enabled: self.long_context_enabled,
        }
    }
}

impl SystemOneClient<StaticCredentialSource> {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_credential_source(StaticCredentialSource::new(api_key))
    }
}

impl<C: CredentialSource> SystemOneClient<C> {
    pub fn with_credential_source(credential_source: C) -> Self {
        let mut definition = ProviderDefinition::new("typesafe");
        definition.base_url = Some("https://api.typesafe.ai".to_string());
        definition.auth_scheme = AuthScheme::BearerToken;
        Self::with_definition(definition, credential_source)
    }

    /// Use an explicitly configured endpoint and credential source.
    pub fn with_definition(definition: ProviderDefinition, credential_source: C) -> Self {
        Self {
            definition,
            credential_source: Arc::new(credential_source),
            client: reqwest::Client::new(),
            images_enabled: false,
            long_context_enabled: false,
        }
    }

    /// Replace the HTTP client, for example to configure a request timeout.
    pub fn with_http_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Opt in to TypeSafe's experimental image and box-question capability.
    pub fn with_images_enabled(mut self) -> Self {
        self.images_enabled = true;
        self
    }

    /// Opt in to TypeSafe's experimental long-context capability.
    pub fn with_long_context_enabled(mut self) -> Self {
        self.long_context_enabled = true;
        self
    }

    /// Evaluate the named questions in one request. This method does not retry.
    pub async fn evaluate(
        &self,
        request: &SystemOneRequest,
    ) -> Result<SystemOneResponse, ProviderError> {
        request.validate(self.images_enabled)?;
        let credentials = self.credential_source.credentials().await?;
        let url = self
            .definition
            .request_url_with_auth_for_path("v1/systemone", &credentials)?;
        let mut headers = self.definition.build_headers(&credentials)?;
        if self.images_enabled {
            headers.insert(
                "typesafe-danger-zone-images",
                http::HeaderValue::from_static("true"),
            );
        }
        if self.long_context_enabled {
            headers.insert(
                "typesafe-danger-zone-long-context",
                http::HeaderValue::from_static("true"),
            );
        }
        let response = self
            .client
            .post(url)
            .headers(headers)
            .json(request)
            .send()
            .await
            .map_err(ProviderError::Transport)?;
        if !response.status().is_success() {
            return Err(ProviderError::from_http_response(response).await);
        }
        let response = response
            .json::<SystemOneResponse>()
            .await
            .map_err(ProviderError::Decode)?;
        response.validate_against(request)?;
        Ok(response)
    }

    /// Discover model names and aliases available to this account.
    pub async fn list_models(&self) -> Result<Vec<SystemOneModel>, ProviderError> {
        let credentials = self.credential_source.credentials().await?;
        let url = self
            .definition
            .request_url_with_auth_for_path("v1/models", &credentials)?;
        let response = self
            .client
            .get(url)
            .headers(self.definition.build_headers(&credentials)?)
            .send()
            .await
            .map_err(ProviderError::Transport)?;
        if !response.status().is_success() {
            return Err(ProviderError::from_http_response(response).await);
        }
        let models = response
            .json::<SystemOneModels>()
            .await
            .map_err(ProviderError::Decode)?;
        Ok(models.models)
    }
}

#[cfg(test)]
mod tests;
