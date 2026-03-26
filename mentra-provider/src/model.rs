use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt::Display;
use time::OffsetDateTime;

/// Metadata describing a model available from a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub provider: crate::ProviderId,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub created_at: Option<OffsetDateTime>,
}

impl ModelInfo {
    pub fn new(id: impl Into<String>, provider: impl Into<crate::ProviderId>) -> Self {
        Self {
            id: id.into(),
            provider: provider.into(),
            display_name: None,
            description: None,
            created_at: None,
        }
    }
}

/// Selection strategy used when resolving a model from a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSelector {
    Id(String),
    NewestAvailable,
}

/// Provider-neutral token usage metadata for a completed or in-progress response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub thoughts_tokens: Option<u64>,
    pub tool_input_tokens: Option<u64>,
}

impl TokenUsage {
    pub fn is_empty(&self) -> bool {
        self.input_tokens.is_none()
            && self.output_tokens.is_none()
            && self.total_tokens.is_none()
            && self.cache_read_input_tokens.is_none()
            && self.cache_creation_input_tokens.is_none()
            && self.reasoning_tokens.is_none()
            && self.thoughts_tokens.is_none()
            && self.tool_input_tokens.is_none()
    }
}

/// Provider-neutral chat role labels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    Unknown(String),
}

impl Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Unknown(role) => role.as_str(),
        };
        f.write_str(value)
    }
}

/// Image payload supported by model providers.
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

/// Tool result payloads supported by provider streams and history replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Structured(Value),
}

impl ToolResultContent {
    pub fn text(value: impl Into<String>) -> Self {
        Self::Text(value.into())
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::Structured(value) => value.to_string().len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&mut self) {
        *self = Self::Text(String::new());
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Text(text) => text.as_str(),
            Self::Structured(_) => panic!("ToolResultContent::as_str requires text content"),
        }
    }

    pub fn contains(&self, pattern: &str) -> bool {
        match self {
            Self::Text(text) => text.contains(pattern),
            Self::Structured(value) => value.to_string().contains(pattern),
        }
    }

    pub fn starts_with(&self, pattern: &str) -> bool {
        match self {
            Self::Text(text) => text.starts_with(pattern),
            Self::Structured(value) => value.to_string().starts_with(pattern),
        }
    }

    pub fn push_str(&mut self, value: &str) {
        match self {
            Self::Text(text) => text.push_str(value),
            Self::Structured(existing) => {
                let mut text = existing.to_string();
                text.push_str(value);
                *self = Self::Text(text);
            }
        }
    }

    pub fn to_display_string(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Structured(value) => value.to_string(),
        }
    }
}

impl Default for ToolResultContent {
    fn default() -> Self {
        Self::Text(String::new())
    }
}

impl From<String> for ToolResultContent {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl Display for ToolResultContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_display_string())
    }
}

impl PartialEq<&str> for ToolResultContent {
    fn eq(&self, other: &&str) -> bool {
        self.to_display_string() == *other
    }
}

impl PartialEq<str> for ToolResultContent {
    fn eq(&self, other: &str) -> bool {
        self.to_display_string() == other
    }
}

impl PartialEq<ToolResultContent> for &str {
    fn eq(&self, other: &ToolResultContent) -> bool {
        *self == other.to_display_string()
    }
}

impl PartialEq<ToolResultContent> for str {
    fn eq(&self, other: &ToolResultContent) -> bool {
        self == other.to_display_string()
    }
}

impl From<&str> for ToolResultContent {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

/// Provider-neutral hosted tool search action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedToolSearchCall {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
}

/// Provider-neutral hosted web search actions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WebSearchAction {
    Search {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        queries: Option<Vec<String>>,
    },
    OpenPage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    FindInPage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
    },
}

/// Provider-neutral hosted web search call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedWebSearchCall {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<WebSearchAction>,
}

/// Provider-neutral image generation result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageGenerationResult {
    Image {
        source: ImageSource,
    },
    ArtifactRef {
        artifact_id: String,
    },
}

/// Provider-neutral image generation call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageGenerationCall {
    pub id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revised_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ImageGenerationResult>,
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
        content: ToolResultContent,
        is_error: bool,
    },
    HostedToolSearch {
        call: HostedToolSearchCall,
    },
    HostedWebSearch {
        call: HostedWebSearchCall,
    },
    ImageGeneration {
        call: ImageGenerationCall,
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

/// Provider-neutral chat message content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user(content: ContentBlock) -> Self {
        Self {
            role: Role::User,
            content: vec![content],
        }
    }

    pub fn assistant(content: ContentBlock) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![content],
        }
    }

    pub fn unknown(role: impl Into<String>, content: ContentBlock) -> Self {
        Self {
            role: Role::Unknown(role.into()),
            content: vec![content],
        }
    }

    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
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
