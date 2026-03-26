use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use http::header;
use serde::Deserialize;
use serde::Serialize;
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Display;
use std::time::Duration;
use strum::Display as StrumDisplay;
use strum::IntoStaticStr;
use url::Url;

use crate::request::SessionRequestOptions;

/// Builtin provider families Mentra can construct from presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, StrumDisplay, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub enum BuiltinProvider {
    Anthropic,
    Gemini,
    OpenAI,
    OpenRouter,
    Ollama,
    LmStudio,
}

impl From<BuiltinProvider> for ProviderId {
    fn from(value: BuiltinProvider) -> Self {
        Self(Cow::Borrowed(value.into()))
    }
}

/// Stable identifier for a registered provider implementation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub struct ProviderId(Cow<'static, str>);

impl ProviderId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(Cow::Owned(id.into()))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

impl Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<&str> for ProviderId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for ProviderId {
    fn from(value: String) -> Self {
        Self(Cow::Owned(value))
    }
}

impl From<&String> for ProviderId {
    fn from(value: &String) -> Self {
        Self::new(value.as_str())
    }
}

/// Human-facing metadata about a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDescriptor {
    pub id: ProviderId,
    pub display_name: Option<String>,
    pub description: Option<String>,
}

impl ProviderDescriptor {
    pub fn new(id: impl Into<ProviderId>) -> Self {
        Self {
            id: id.into(),
            display_name: None,
            description: None,
        }
    }
}

/// Capabilities advertised by a provider instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProviderCapabilities {
    pub supports_model_listing: bool,
    pub supports_streaming: bool,
    pub supports_websockets: bool,
    pub supports_tool_calls: bool,
    pub supports_images: bool,
    pub supports_history_compaction: bool,
    pub supports_deferred_tools: bool,
    pub supports_hosted_tool_search: bool,
    pub supports_hosted_web_search: bool,
    pub supports_image_generation: bool,
    pub supports_reasoning_effort: bool,
    pub reports_reasoning_tokens: bool,
    pub reports_thoughts_tokens: bool,
    pub supports_structured_tool_results: bool,
}

/// Wire protocol supported by a provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WireApi {
    #[default]
    Responses,
    AnthropicMessages,
    GeminiGenerateContent,
}

impl Display for WireApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Responses => "responses",
            Self::AnthropicMessages => "anthropic_messages",
            Self::GeminiGenerateContent => "gemini_generate_content",
        };
        f.write_str(value)
    }
}

/// Retry configuration for provider transport calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub max_attempts: u64,
    pub base_delay: Duration,
    pub retry_429: bool,
    pub retry_5xx: bool,
    pub retry_transport: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_delay: Duration::from_millis(200),
            retry_429: false,
            retry_5xx: true,
            retry_transport: true,
        }
    }
}

/// Serializable provider definition used by runtime and adapter layers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDefinition {
    pub descriptor: ProviderDescriptor,
    #[serde(default)]
    pub wire_api: WireApi,
    #[serde(default)]
    pub auth_scheme: crate::AuthScheme,
    #[serde(default)]
    pub capabilities: ProviderCapabilities,
    pub base_url: Option<String>,
    #[serde(default)]
    pub query_params: Option<HashMap<String, String>>,
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
    #[serde(default)]
    pub retry: RetryPolicy,
    #[serde(default = "default_stream_idle_timeout")]
    pub stream_idle_timeout: Duration,
    #[serde(default = "default_websocket_connect_timeout")]
    pub websocket_connect_timeout: Duration,
}

fn default_stream_idle_timeout() -> Duration {
    Duration::from_millis(300_000)
}

fn default_websocket_connect_timeout() -> Duration {
    Duration::from_millis(15_000)
}

impl ProviderDefinition {
    pub fn new(id: impl Into<ProviderId>) -> Self {
        Self {
            descriptor: ProviderDescriptor::new(id),
            wire_api: WireApi::default(),
            auth_scheme: crate::AuthScheme::default(),
            capabilities: ProviderCapabilities {
                supports_model_listing: true,
                supports_streaming: true,
                supports_websockets: false,
                supports_tool_calls: true,
                supports_images: true,
                supports_history_compaction: false,
                supports_deferred_tools: false,
                supports_hosted_tool_search: false,
                supports_hosted_web_search: false,
                supports_image_generation: false,
                supports_reasoning_effort: false,
                reports_reasoning_tokens: false,
                reports_thoughts_tokens: false,
                supports_structured_tool_results: false,
            },
            base_url: None,
            query_params: None,
            headers: None,
            retry: RetryPolicy::default(),
            stream_idle_timeout: default_stream_idle_timeout(),
            websocket_connect_timeout: default_websocket_connect_timeout(),
        }
    }

    pub fn descriptor(&self) -> ProviderDescriptor {
        self.descriptor.clone()
    }

    pub fn provider_id(&self) -> &ProviderId {
        &self.descriptor.id
    }

    pub fn url_for_path(&self, path: &str) -> String {
        let base = self
            .base_url
            .as_deref()
            .unwrap_or_default()
            .trim_end_matches('/');
        let path = path.trim_start_matches('/');
        let mut url = if path.is_empty() {
            base.to_string()
        } else {
            format!("{base}/{path}")
        };

        if let Some(params) = &self.query_params
            && !params.is_empty()
        {
            let qs = params
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join("&");
            url.push('?');
            url.push_str(&qs);
        }

        url
    }

    pub fn build_headers(
        &self,
        credentials: &crate::ProviderCredentials,
    ) -> Result<HeaderMap, crate::ProviderError> {
        let mut headers = HeaderMap::new();

        if let Some(configured_headers) = &self.headers {
            for (name, value) in configured_headers {
                insert_header(&mut headers, name, value)?;
            }
        }

        for (name, value) in &credentials.headers {
            insert_header(&mut headers, name, value)?;
        }

        match &self.auth_scheme {
            crate::AuthScheme::None | crate::AuthScheme::QueryParam { .. } => {}
            crate::AuthScheme::BearerToken => {
                let token = required_auth_value(credentials)?;
                let auth_value =
                    HeaderValue::from_str(&format!("Bearer {token}")).map_err(|error| {
                        crate::ProviderError::InvalidRequest(format!(
                            "invalid bearer token header: {error}"
                        ))
                    })?;
                headers.insert(header::AUTHORIZATION, auth_value);
            }
            crate::AuthScheme::Header { name } => {
                let token = required_auth_value(credentials)?;
                insert_header(&mut headers, name, token)?;
            }
        }

        Ok(headers)
    }

    pub fn build_headers_for_session(
        &self,
        credentials: &crate::ProviderCredentials,
        session: Option<&SessionRequestOptions>,
        fallback_turn_state: Option<&str>,
    ) -> Result<HeaderMap, crate::ProviderError> {
        let mut headers = self.build_headers(credentials)?;

        if let Some(turn_state) = session
            .and_then(|session| session.sticky_turn_state.as_deref())
            .or(fallback_turn_state)
            && let Ok(value) = HeaderValue::from_str(turn_state)
        {
            headers.insert("x-mentra-turn-state", value.clone());
            headers.insert("x-codex-turn-state", value);
        }
        if let Some(value) = session.and_then(|session| session.turn_metadata.as_deref())
            && let Ok(value) = HeaderValue::from_str(value)
        {
            headers.insert("x-mentra-turn-metadata", value.clone());
            headers.insert("x-codex-turn-metadata", value);
        }
        if let Some(value) = session.and_then(|session| session.session_affinity.as_deref())
            && let Ok(value) = HeaderValue::from_str(value)
        {
            headers.insert("x-mentra-session-affinity", value);
        }
        if let Some(prefer_connection_reuse) =
            session.and_then(|session| session.prefer_connection_reuse)
        {
            headers.insert(
                "x-mentra-connection-reuse",
                HeaderValue::from_static(if prefer_connection_reuse {
                    "prefer-reuse"
                } else {
                    "prefer-fresh"
                }),
            );
        }

        Ok(headers)
    }

    pub fn request_url_with_auth_for_path(
        &self,
        path: &str,
        credentials: &crate::ProviderCredentials,
    ) -> Result<Url, crate::ProviderError> {
        let mut url = Url::parse(&self.url_for_path(path))
            .map_err(|error| crate::ProviderError::InvalidRequest(error.to_string()))?;

        if let crate::AuthScheme::QueryParam { name } = &self.auth_scheme {
            let token = required_auth_value(credentials)?;
            url.query_pairs_mut().append_pair(name, token);
        }

        Ok(url)
    }

    pub fn websocket_url_for_path(&self, path: &str) -> Result<Url, url::ParseError> {
        let mut url = Url::parse(&self.url_for_path(path))?;

        let scheme = match url.scheme() {
            "http" => "ws",
            "https" => "wss",
            "ws" | "wss" => return Ok(url),
            _ => return Ok(url),
        };
        let _ = url.set_scheme(scheme);
        Ok(url)
    }

    pub fn websocket_url_with_auth_for_path(
        &self,
        path: &str,
        credentials: &crate::ProviderCredentials,
    ) -> Result<Url, crate::ProviderError> {
        let mut url = self
            .websocket_url_for_path(path)
            .map_err(|error| crate::ProviderError::InvalidRequest(error.to_string()))?;

        if let crate::AuthScheme::QueryParam { name } = &self.auth_scheme {
            let token = required_auth_value(credentials)?;
            url.query_pairs_mut().append_pair(name, token);
        }

        Ok(url)
    }
}

fn insert_header(
    headers: &mut HeaderMap,
    name: &str,
    value: &str,
) -> Result<(), crate::ProviderError> {
    let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
        crate::ProviderError::InvalidRequest(format!(
            "invalid provider header name {name:?}: {error}"
        ))
    })?;
    let header_value = HeaderValue::from_str(value).map_err(|error| {
        crate::ProviderError::InvalidRequest(format!(
            "invalid provider header value for {name:?}: {error}"
        ))
    })?;
    headers.insert(header_name, header_value);
    Ok(())
}

fn required_auth_value(
    credentials: &crate::ProviderCredentials,
) -> Result<&str, crate::ProviderError> {
    credentials.bearer_token.as_deref().ok_or_else(|| {
        crate::ProviderError::InvalidRequest("missing provider auth credential".to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_headers_applies_bearer_auth_and_static_headers() {
        let mut definition = ProviderDefinition::new("test");
        definition.auth_scheme = crate::AuthScheme::BearerToken;
        definition.headers = Some(HashMap::from([(
            "x-provider-header".to_string(),
            "static".to_string(),
        )]));

        let headers = definition
            .build_headers(&crate::ProviderCredentials {
                bearer_token: Some("secret".to_string()),
                account_id: None,
                headers: HashMap::from([("x-runtime-header".to_string(), "dynamic".to_string())]),
            })
            .expect("headers should build");

        assert_eq!(headers["x-provider-header"], "static");
        assert_eq!(headers["x-runtime-header"], "dynamic");
        assert_eq!(headers[header::AUTHORIZATION], "Bearer secret");
    }

    #[test]
    fn request_url_with_auth_appends_query_param_auth() {
        let mut definition = ProviderDefinition::new("test");
        definition.base_url = Some("https://example.com/v1".to_string());
        definition.query_params = Some(HashMap::from([(
            "api-version".to_string(),
            "2026".to_string(),
        )]));
        definition.auth_scheme = crate::AuthScheme::QueryParam {
            name: "api-key".to_string(),
        };

        let url = definition
            .request_url_with_auth_for_path(
                "responses",
                &crate::ProviderCredentials {
                    bearer_token: Some("secret".to_string()),
                    account_id: None,
                    headers: HashMap::new(),
                },
            )
            .expect("url should build");

        assert_eq!(
            url.as_str(),
            "https://example.com/v1/responses?api-version=2026&api-key=secret"
        );
    }
}
