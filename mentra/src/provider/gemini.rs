use async_trait::async_trait;
use url::Url;

mod model;
mod sse;

use crate::provider::{
    Provider,
    model::{
        ModelInfo, ProviderDescriptor, ProviderError, ProviderEventStream, ProviderId, Request,
    },
};

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/";

#[derive(Clone)]
pub struct GeminiProvider {
    client: reqwest::Client,
    base_url: Url,
}

impl GeminiProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "x-goog-api-key",
            api_key
                .into()
                .parse()
                .expect("Failed to parse Gemini API key"),
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
impl Provider for GeminiProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: ProviderId::GEMINI,
            display_name: Some("Gemini".to_string()),
            description: Some("Google Gemini Developer API provider".to_string()),
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let mut models = Vec::new();
        let mut page_token = None::<String>;

        loop {
            let mut request = self
                .client
                .get(
                    self.base_url
                        .join("v1beta/models")
                        .expect("Failed to join Gemini models URL"),
                )
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

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let model_name = request.model.to_string();
        let request = model::GeminiGenerateContentRequest::try_from(request)?;
        let response = self
            .client
            .post(
                self.base_url
                    .join(&format!(
                        "v1beta/{}:streamGenerateContent",
                        normalize_model_name(&model_name)
                    ))
                    .expect("Failed to join Gemini stream URL"),
            )
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
}

fn normalize_model_name(model: &str) -> String {
    if model.starts_with("models/") {
        model.to_string()
    } else {
        format!("models/{model}")
    }
}
