//! An explicitly invoked client for OpenAI's Decisions API.
//!
//! Decisions are separate from the agent's model/tool loop. They do not run
//! automatically and their usage is not included in an agent run's budget.

mod model;

use std::sync::Arc;

pub use model::*;

use crate::{CredentialSource, ProviderDefinition, ProviderError, StaticCredentialSource};

/// OpenAI model documented for the Decisions API at the time of this release.
/// Callers can supply a different model in the request as the API evolves.
pub const DEFAULT_DECISIONS_MODEL: &str = "gpt-6-luna";

/// Client for `POST /v1/decisions`, with its own credentials and HTTP settings.
///
/// The default constructor targets the OpenAI API. A custom definition can be
/// supplied explicitly for a compatible gateway. Clones share the HTTP pool
/// and credential source, but no conversation or agent state.
pub struct DecisionsClient<C> {
    definition: ProviderDefinition,
    credential_source: Arc<C>,
    client: reqwest::Client,
}

impl<C> Clone for DecisionsClient<C> {
    fn clone(&self) -> Self {
        Self {
            definition: self.definition.clone(),
            credential_source: Arc::clone(&self.credential_source),
            client: self.client.clone(),
        }
    }
}

impl DecisionsClient<StaticCredentialSource> {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_credential_source(StaticCredentialSource::new(api_key))
    }
}

impl<C: CredentialSource> DecisionsClient<C> {
    pub fn with_credential_source(credential_source: C) -> Self {
        Self::with_definition(crate::responses::openai_definition(), credential_source)
    }

    /// Use an explicitly configured endpoint and credential source.
    pub fn with_definition(definition: ProviderDefinition, credential_source: C) -> Self {
        Self {
            definition,
            credential_source: Arc::new(credential_source),
            client: reqwest::Client::new(),
        }
    }

    /// Replace the HTTP client, for example to configure a request timeout.
    pub fn with_http_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Evaluate the supplied questions. This sends one request and does not
    /// retry or translate a refusal into a negative answer.
    pub async fn create(
        &self,
        request: &DecisionRequest,
    ) -> Result<DecisionResponse, ProviderError> {
        request.validate()?;
        let credentials = self.credential_source.credentials().await?;
        let url = self
            .definition
            .request_url_with_auth_for_path("v1/decisions", &credentials)?;
        let response = self
            .client
            .post(url)
            .headers(self.definition.build_headers(&credentials)?)
            .json(request)
            .send()
            .await
            .map_err(ProviderError::Transport)?;

        if !response.status().is_success() {
            return Err(ProviderError::from_http_response(response).await);
        }

        let response = response
            .json::<DecisionResponse>()
            .await
            .map_err(ProviderError::Decode)?;
        response.validate_against(request)?;
        Ok(response)
    }
}

#[cfg(test)]
mod tests;
