use async_trait::async_trait;
use serde_json::Value;
use url::Url;

use crate::{
    BuiltinProvider,
    provider::{
        Provider,
        model::{ModelInfo, ProviderDescriptor, ProviderError, ProviderEventStream, Request},
        openai::{model, sse},
    },
};

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/";

#[derive(Clone)]
pub struct OpenRouterProvider {
    client: reqwest::Client,
    base_url: Url,
    api_key: String,
}

impl OpenRouterProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .build()
                .expect("Failed to build client"),
            base_url: Url::parse(DEFAULT_BASE_URL).expect("Failed to parse default base URL"),
            api_key: api_key.into(),
        }
    }

    async fn send_response(
        &self,
        request: Request<'_>,
        stream: bool,
    ) -> Result<reqwest::Response, ProviderError> {
        let request = model::OpenAIResponsesRequest::try_from_request(request, "OpenRouter")?;
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
            .bearer_auth(&self.api_key)
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

#[async_trait]
impl Provider for OpenRouterProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: BuiltinProvider::OpenRouter.into(),
            display_name: Some("OpenRouter".to_string()),
            description: Some("OpenRouter Responses API provider".to_string()),
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
            .bearer_auth(&self.api_key)
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

        Ok(models.into_model_info(BuiltinProvider::OpenRouter))
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let response = self.send_response(request, true).await?;
        Ok(sse::spawn_event_stream(response))
    }
}
