use async_trait::async_trait;
use serde_json::Value;
use url::Url;

mod model;
mod sse;

use crate::provider::{
    Provider,
    model::{
        ModelInfo, ProviderDescriptor, ProviderError, ProviderEventStream, ProviderId, Request,
    },
};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/";

#[derive(Clone)]
pub struct OpenAIProvider {
    client: reqwest::Client,
    base_url: Url,
}

impl OpenAIProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        let api_key = api_key.into();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {api_key}")
                .parse()
                .expect("Failed to parse OpenAI authorization header"),
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
impl Provider for OpenAIProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: ProviderId::OPENAI,
            display_name: Some("OpenAI".to_string()),
            description: Some("OpenAI Responses API provider".to_string()),
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let response = self
            .client
            .get(
                self.base_url
                    .join("v1/models")
                    .expect("Failed to join models URL"),
            )
            .send()
            .await
            .map_err(ProviderError::Transport)?;

        if !response.status().is_success() {
            return Err(ProviderError::Http {
                status: response.status(),
                body: response.text().await.unwrap_or_default(),
            });
        }

        let models = response
            .json::<model::OpenAIModelsPage>()
            .await
            .map_err(ProviderError::Decode)?;

        Ok(models.data.into_iter().map(Into::into).collect())
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let response = self.send_response(request, true).await?;
        Ok(sse::spawn_event_stream(response))
    }
}

impl OpenAIProvider {
    async fn send_response(
        &self,
        request: Request<'_>,
        stream: bool,
    ) -> Result<reqwest::Response, ProviderError> {
        let request = model::OpenAIResponsesRequest::try_from(request)?;
        let mut body = serde_json::to_value(request).map_err(ProviderError::Serialize)?;
        if stream {
            body["stream"] = Value::Bool(true);
        }

        let response = self
            .client
            .post(
                self.base_url
                    .join("v1/responses")
                    .expect("Failed to join responses URL"),
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
