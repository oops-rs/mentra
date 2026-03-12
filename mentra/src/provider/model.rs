mod response_builder;
mod stream;

use std::{
    borrow::Cow,
    collections::BTreeMap,
    error::Error,
    fmt::{self, Display, Formatter},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

use crate::tool::ToolSpec;

pub use response_builder::collect_response_from_stream;
pub use stream::{
    ContentBlockDelta, ContentBlockStart, ProviderEvent, ProviderEventStream,
    provider_event_stream_from_response,
};

/// Builtin model providers that Mentra can construct from API keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinProvider {
    Anthropic,
    OpenAI,
    Gemini,
}

impl From<BuiltinProvider> for ProviderId {
    fn from(value: BuiltinProvider) -> Self {
        match value {
            BuiltinProvider::Anthropic => ProviderId::ANTHROPIC,
            BuiltinProvider::OpenAI => ProviderId::OPENAI,
            BuiltinProvider::Gemini => ProviderId::GEMINI,
        }
    }
}

/// Stable identifier for a registered provider implementation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub struct ProviderId(Cow<'static, str>);

impl ProviderId {
    pub const ANTHROPIC: Self = ProviderId(Cow::Borrowed("anthropic"));
    pub const OPENAI: Self = ProviderId(Cow::Borrowed("openai"));
    pub const GEMINI: Self = ProviderId(Cow::Borrowed("gemini"));
}

impl ProviderId {
    /// Creates a provider identifier from a runtime string.
    pub fn new(id: impl Into<String>) -> Self {
        Self(Cow::Owned(id.into()))
    }

    /// Returns the provider identifier as a string slice.
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
    /// Creates a provider descriptor with only an identifier.
    pub fn new(id: impl Into<ProviderId>) -> Self {
        Self {
            id: id.into(),
            display_name: None,
            description: None,
        }
    }
}

/// Metadata describing a model available from a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub provider: ProviderId,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub created_at: Option<OffsetDateTime>,
}

/// Errors returned by provider implementations and stream adapters.
#[derive(Debug)]
pub enum ProviderError {
    Transport(reqwest::Error),
    Http {
        status: reqwest::StatusCode,
        body: String,
    },
    Decode(reqwest::Error),
    Serialize(serde_json::Error),
    Deserialize(serde_json::Error),
    InvalidRequest(String),
    InvalidResponse(String),
    MalformedStream(String),
}

impl Display for ProviderError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "provider transport error: {error}"),
            Self::Http { status, body } if body.trim().is_empty() => {
                write!(f, "provider returned HTTP {status}")
            }
            Self::Http { status, body } => write!(f, "provider returned HTTP {status}: {body}"),
            Self::Decode(error) => write!(f, "failed to decode provider response: {error}"),
            Self::Serialize(error) => write!(f, "failed to serialize provider request: {error}"),
            Self::Deserialize(error) => {
                write!(f, "failed to deserialize provider payload: {error}")
            }
            Self::InvalidRequest(message) => write!(f, "invalid provider request: {message}"),
            Self::InvalidResponse(message) => write!(f, "invalid provider response: {message}"),
            Self::MalformedStream(message) => write!(f, "malformed provider stream: {message}"),
        }
    }
}

impl Error for ProviderError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transport(error) | Self::Decode(error) => Some(error),
            Self::Serialize(error) | Self::Deserialize(error) => Some(error),
            _ => None,
        }
    }
}

/// Provider request assembled by the runtime before dispatch.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub model: Cow<'a, str>,
    pub system: Option<Cow<'a, str>>,
    pub messages: Cow<'a, [Message]>,
    pub tools: Cow<'a, [ToolSpec]>,
    pub tool_choice: Option<ToolChoice>,
    pub temperature: Option<f32>,
    pub max_output_tokens: Option<u32>,
    pub metadata: Cow<'a, BTreeMap<String, String>>,
}

impl Request<'_> {
    /// Converts borrowed request fields into owned values.
    pub fn into_owned(self) -> Request<'static> {
        Request {
            model: Cow::Owned(self.model.into_owned()),
            system: self.system.map(|system| Cow::Owned(system.into_owned())),
            messages: Cow::Owned(self.messages.into_owned()),
            tools: Cow::Owned(self.tools.into_owned()),
            tool_choice: self.tool_choice,
            temperature: self.temperature,
            max_output_tokens: self.max_output_tokens,
            metadata: Cow::Owned(self.metadata.into_owned()),
        }
    }
}

/// A complete response collected from a provider stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    pub model: String,
    pub role: Role,
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<String>,
}

/// Provider-neutral chat role labels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
    Unknown(String),
}

/// Provider-neutral chat message content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    /// Creates a single-block user message.
    pub fn user(content: ContentBlock) -> Self {
        Self {
            role: Role::User,
            content: vec![content],
        }
    }

    /// Creates a single-block assistant message.
    pub fn assistant(content: ContentBlock) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![content],
        }
    }

    /// Creates a message with a provider-specific role label.
    pub fn unknown(role: impl Into<String>, content: ContentBlock) -> Self {
        Self {
            role: Role::Unknown(role.into()),
            content: vec![content],
        }
    }
}

/// A provider-neutral content block exchanged with models.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        source: ImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

impl ContentBlock {
    /// Creates a text content block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Creates an inline image content block from raw bytes.
    pub fn image_bytes(media_type: impl Into<String>, data: impl Into<Vec<u8>>) -> Self {
        Self::Image {
            source: ImageSource::bytes(media_type, data),
        }
    }

    /// Creates an image content block referencing a remote URL.
    pub fn image_url(url: impl Into<String>) -> Self {
        Self::Image {
            source: ImageSource::url(url),
        }
    }
}

/// Image payload supported by model providers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageSource {
    Bytes { media_type: String, data: Vec<u8> },
    Url { url: String },
}

impl ImageSource {
    /// Creates an inline image source from bytes.
    pub fn bytes(media_type: impl Into<String>, data: impl Into<Vec<u8>>) -> Self {
        Self::Bytes {
            media_type: media_type.into(),
            data: data.into(),
        }
    }

    /// Creates a URL-backed image source.
    pub fn url(url: impl Into<String>) -> Self {
        Self::Url { url: url.into() }
    }
}

/// Provider-neutral tool choice hint passed to model APIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ToolChoice {
    #[default]
    Auto,
    Any,
    Tool {
        name: String,
    },
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::{ProviderError, ProviderId};

    #[test]
    fn provider_id_new_accepts_runtime_strings() {
        let id = ProviderId::new(format!("custom-{}", "provider"));

        assert_eq!(id.as_str(), "custom-provider");
    }

    #[test]
    fn provider_error_display_includes_http_status() {
        let error = ProviderError::Http {
            status: reqwest::StatusCode::BAD_REQUEST,
            body: "bad payload".to_string(),
        };

        assert_eq!(
            error.to_string(),
            "provider returned HTTP 400 Bad Request: bad payload"
        );
    }

    #[test]
    fn provider_error_exposes_source_for_serde_failures() {
        let error = ProviderError::Serialize(
            serde_json::from_str::<serde_json::Value>("{").expect_err("invalid json"),
        );

        assert!(error.source().is_some());
    }
}
