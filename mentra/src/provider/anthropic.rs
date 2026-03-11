use async_trait::async_trait;

use serde_json::Value;
use url::Url;

mod model;
mod sse;
mod stream_model;

use crate::provider::{
    Provider,
    model::{ModelInfo, ModelProviderKind, ProviderError, ProviderEventStream, Request},
};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Clone)]
pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: Url,
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "x-api-key",
            api_key.into().parse().expect("Failed to parse API key"),
        );
        headers.insert(
            "anthropic-version",
            ANTHROPIC_VERSION
                .parse()
                .expect("Failed to parse Anthropic version"),
        );

        Self {
            client: reqwest::Client::builder()
                .default_headers(headers)
                .build()
                .expect("Failed to build client"),
            base_url: Url::parse(DEFAULT_BASE_URL).expect("Failed to parse default base URL"),
        }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn kind(&self) -> ModelProviderKind {
        ModelProviderKind::Anthropic
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let mut models = Vec::new();
        let mut after_id = None;

        loop {
            let response = self
                .client
                .get(
                    self.base_url
                        .join("v1/models")
                        .expect("Failed to join models URL"),
                )
                .query(&[
                    ("limit", "1000"),
                    ("after_id", after_id.as_deref().unwrap_or("")),
                ])
                .send()
                .await
                .map_err(ProviderError::Transport)?;

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

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let response = self.send_message(request, true).await?;
        Ok(sse::spawn_event_stream(response))
    }
}

impl AnthropicProvider {
    async fn send_message(
        &self,
        request: Request<'_>,
        stream: bool,
    ) -> Result<reqwest::Response, ProviderError> {
        let request = model::AnthropicRequest::try_from(request)?;
        let mut body = serde_json::to_value(request).map_err(ProviderError::Serialize)?;
        if stream {
            body["stream"] = Value::Bool(true);
        }
        let response = self
            .client
            .post(
                self.base_url
                    .join("v1/messages")
                    .expect("Failed to join messages URL"),
            )
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
