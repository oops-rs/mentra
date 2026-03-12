mod response_builder;
mod stream;

use std::{borrow::Cow, collections::BTreeMap, fmt::Display};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

use crate::tool::ToolSpec;

pub use response_builder::collect_response_from_stream;
pub use stream::{
    ContentBlockDelta, ContentBlockStart, ProviderEvent, ProviderEventStream,
    provider_event_stream_from_response,
};

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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub struct ProviderId(Cow<'static, str>);

impl ProviderId {
    pub const ANTHROPIC: Self = ProviderId(Cow::Borrowed("anthropic"));
    pub const OPENAI: Self = ProviderId(Cow::Borrowed("openai"));
    pub const GEMINI: Self = ProviderId(Cow::Borrowed("gemini"));
}

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

#[cfg(test)]
mod tests {
    use super::ProviderId;

    #[test]
    fn provider_id_new_accepts_runtime_strings() {
        let id = ProviderId::new(format!("custom-{}", "provider"));

        assert_eq!(id.as_str(), "custom-provider");
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub provider: ProviderId,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub created_at: Option<OffsetDateTime>,
}

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    pub model: String,
    pub role: Role,
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

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
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn image_bytes(media_type: impl Into<String>, data: impl Into<Vec<u8>>) -> Self {
        Self::Image {
            source: ImageSource::bytes(media_type, data),
        }
    }

    pub fn image_url(url: impl Into<String>) -> Self {
        Self::Image {
            source: ImageSource::url(url),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageSource {
    Bytes { media_type: String, data: Vec<u8> },
    Url { url: String },
}

impl ImageSource {
    pub fn bytes(media_type: impl Into<String>, data: impl Into<Vec<u8>>) -> Self {
        Self::Bytes {
            media_type: media_type.into(),
            data: data.into(),
        }
    }

    pub fn url(url: impl Into<String>) -> Self {
        Self::Url { url: url.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ToolChoice {
    #[default]
    Auto,
    Any,
    Tool {
        name: String,
    },
}
