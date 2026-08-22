//! The OpenAI `chat/completions` wire.
//!
//! `v1/responses` is OpenAI's own current wire and almost nothing else speaks
//! it. `chat/completions` is what the rest of the ecosystem implements —
//! DeepSeek, Groq, Together, Fireworks, OpenRouter, Mistral, xAI, vLLM,
//! llama.cpp, Ollama, LM Studio — so a provider reachable only over
//! `v1/responses` is a provider that answers 404 on almost every base URL a
//! host might point it at.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

pub(crate) mod model;
pub(crate) mod sse;

use crate::AuthScheme;
use crate::CredentialSource;
use crate::ModelCatalog;
use crate::ModelInfo;
use crate::ProviderCapabilities;
use crate::ProviderDefinition;
use crate::ProviderError;
use crate::ProviderEventStream;
use crate::ProviderId;
use crate::ProviderSession;
use crate::ProviderSessionFactory;
use crate::RegisteredProvider;
use crate::Request;
use crate::StaticCredentialSource;
use crate::WireApi;

use model::{ChatCompletionsRequest, ChatModelsPage};

const CHAT_COMPLETIONS_PATH: &str = "v1/chat/completions";
const MODELS_PATH: &str = "v1/models";

/// Returns a definition for an endpoint speaking the `chat/completions` wire.
///
/// The caller supplies the provider id and base URL, because this wire has no
/// single vendor: it is the shape a few dozen endpoints agree on.
pub fn definition(
    provider: impl Into<ProviderId>,
    base_url: impl Into<String>,
) -> ProviderDefinition {
    let mut definition = ProviderDefinition::new(provider);
    definition.wire_api = WireApi::OpenAiChatCompletions;
    definition.auth_scheme = AuthScheme::BearerToken;
    definition.capabilities = ProviderCapabilities {
        supports_model_listing: true,
        supports_streaming: true,
        supports_websockets: false,
        supports_tool_calls: true,
        supports_images: true,
        supports_history_compaction: false,
        supports_memory_summarization: false,
        supports_deferred_tools: false,
        supports_hosted_tool_search: false,
        supports_hosted_web_search: false,
        supports_image_generation: false,
        // `reasoning_effort` is accepted by the reasoning models on this wire
        // and ignored by the rest, which is the behavior a host wants either
        // way.
        supports_reasoning_effort: true,
        // Reported by the reasoning endpoints via
        // `completion_tokens_details.reasoning_tokens`; absent elsewhere, which
        // reads as zero rather than as wrong.
        reports_reasoning_tokens: true,
        reports_thoughts_tokens: false,
        supports_structured_tool_results: false,
        supports_embeddings: true,
    };
    definition.base_url = Some(base_url.into());
    definition.headers = Some(HashMap::new());
    definition
}

pub struct ChatCompletionsProvider<C = StaticCredentialSource> {
    client: reqwest::Client,
    credential_source: Arc<C>,
    definition: ProviderDefinition,
}

impl<C> Clone for ChatCompletionsProvider<C> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            credential_source: Arc::clone(&self.credential_source),
            definition: self.definition.clone(),
        }
    }
}

impl<C> ChatCompletionsProvider<C>
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
        // run for minutes, but a gap between chunks means the endpoint stopped
        // talking.
        let client = reqwest::Client::builder()
            .read_timeout(definition.stream_idle_timeout)
            .build()
            .expect("failed to build reqwest client");

        Self {
            client,
            credential_source,
            definition,
        }
    }

    pub fn definition(&self) -> &ProviderDefinition {
        &self.definition
    }

    async fn send_completion(
        &self,
        request: Request<'_>,
        stream: bool,
    ) -> Result<reqwest::Response, ProviderError> {
        let body = ChatCompletionsRequest::from_request(request, stream)?;
        let credentials = self.credential_source.credentials().await?;
        let response = self
            .client
            .post(
                self.definition
                    .request_url_with_auth_for_path(CHAT_COMPLETIONS_PATH, &credentials)?,
            )
            .headers(self.definition.build_headers(&credentials)?)
            .json(&body)
            .send()
            .await
            .map_err(ProviderError::Transport)?;

        if !response.status().is_success() {
            return Err(ProviderError::from_http_response(response).await);
        }

        Ok(response)
    }
}

#[async_trait]
impl<C> ModelCatalog for ChatCompletionsProvider<C>
where
    C: CredentialSource + 'static,
{
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let credentials = self.credential_source.credentials().await?;
        let response = self
            .client
            .get(
                self.definition
                    .request_url_with_auth_for_path(MODELS_PATH, &credentials)?,
            )
            .headers(self.definition.build_headers(&credentials)?)
            .send()
            .await
            .map_err(ProviderError::Transport)?;

        if !response.status().is_success() {
            return Err(ProviderError::from_http_response(response).await);
        }

        let page = response
            .json::<ChatModelsPage>()
            .await
            .map_err(ProviderError::Decode)?;
        let provider = self.definition.provider_id().clone();

        Ok(page
            .data
            .into_iter()
            .map(|model| {
                let mut info = ModelInfo::new(model.id, provider.clone());
                info.created_at = model
                    .created
                    .and_then(|created| time::OffsetDateTime::from_unix_timestamp(created).ok());
                info
            })
            .collect())
    }
}

#[async_trait]
impl<C> ProviderSessionFactory for ChatCompletionsProvider<C>
where
    C: CredentialSource + 'static,
{
    async fn create_session(&self) -> Result<Box<dyn ProviderSession>, ProviderError> {
        Ok(Box::new((*self).clone()))
    }
}

#[async_trait]
impl<C> ProviderSession for ChatCompletionsProvider<C>
where
    C: CredentialSource + 'static,
{
    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let requested_model = request.model.to_string();
        let provider = self.definition.provider_id().clone();
        let response = self.send_completion(request, true).await?;
        Ok(sse::spawn_event_stream(response, provider, requested_model))
    }
}

#[async_trait]
impl<C> RegisteredProvider for ChatCompletionsProvider<C>
where
    C: CredentialSource + 'static,
{
    fn definition(&self) -> ProviderDefinition {
        self.definition.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    use super::*;
    use crate::{
        ContentBlock, Message, ProviderRequestOptions, StaticCredentialSource, ToolChoice,
    };

    /// Serves one SSE response and hands back the raw request it was sent.
    fn spawn_sse_server(body: &'static str) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("read listener addr");

        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut buffer = Vec::new();
            let mut temp = [0_u8; 4096];
            let mut content_length = 0_usize;
            let mut header_end = None;

            loop {
                let read = stream.read(&mut temp).expect("read request");
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&temp[..read]);
                if header_end.is_none()
                    && let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    let end = index + 4;
                    header_end = Some(end);
                    content_length = String::from_utf8_lossy(&buffer[..end])
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().expect("parse length"))
                        })
                        .unwrap_or_default();
                }
                if let Some(end) = header_end
                    && buffer.len() >= end + content_length
                {
                    break;
                }
            }

            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .expect("write response");
            stream.flush().expect("flush response");

            String::from_utf8_lossy(&buffer).into_owned()
        });

        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn a_turn_goes_to_chat_completions_and_comes_back_as_a_response() {
        let (base_url, server) = spawn_sse_server(concat!(
            "data: {\"id\":\"chatcmpl-9\",\"model\":\"served\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"pong\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ));

        let provider = ChatCompletionsProvider::new(
            definition("local", base_url),
            StaticCredentialSource::new("test-key"),
        );

        let response = ProviderSession::send(
            &provider,
            Request {
                model: Cow::Borrowed("a-model"),
                system: None,
                messages: Cow::Owned(vec![Message::user(ContentBlock::text("ping"))]),
                tools: Cow::Owned(vec![]),
                tool_choice: Some(ToolChoice::Auto),
                temperature: None,
                max_output_tokens: Some(64),
                metadata: Cow::Owned(BTreeMap::new()),
                provider_request_options: ProviderRequestOptions::default(),
            },
        )
        .await
        .expect("the turn completes");

        assert_eq!(response.content, vec![ContentBlock::text("pong")]);

        let request = server.join().expect("server thread");
        // The path is the whole point: an endpoint on this wire has no
        // `v1/responses` to answer.
        assert!(
            request.starts_with("POST /v1/chat/completions "),
            "{request}"
        );
        assert!(
            request.contains("authorization: Bearer test-key"),
            "{request}"
        );
        assert!(request.contains("\"stream\":true"), "{request}");
    }
}
