mod response_builder;
mod stream;

use std::{borrow::Cow, collections::BTreeMap};

use serde_json::Value;

use crate::tool::ToolSpec;

pub use response_builder::collect_response_from_stream;
pub use stream::{
    ContentBlockDelta, ContentBlockStart, ProviderEvent, ProviderEventStream,
    provider_event_stream_from_response,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModelProviderKind {
    Anthropic,
    OpenAI,
    Gemini,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub provider: ModelProviderKind,
    pub display_name: Option<String>,
    pub description: Option<String>,
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

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub id: String,
    pub model: String,
    pub role: Role,
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentBlock {
    Text {
        text: String,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChoice {
    Auto,
    Any,
    Tool { name: String },
}
