use serde::{Deserialize, Serialize};
use std::{borrow::Cow, collections::BTreeMap};

use crate::{model::Message, model::ToolChoice, tool::ToolSpec};

/// Provider-neutral reasoning controls supported across multiple providers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningOptions {
    pub effort: ReasoningEffort,
}

/// Shared reasoning effort levels supported by Mentra's public API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

/// Provider-neutral tool search behavior requested for a model call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ToolSearchMode {
    #[default]
    Disabled,
    Hosted,
}

/// Shared Responses-family request options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ResponsesRequestOptions {
    #[serde(default)]
    pub parallel_tool_calls: Option<bool>,
}

/// Anthropic-specific request options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AnthropicRequestOptions {
    #[serde(default)]
    pub disable_parallel_tool_use: Option<bool>,
}

/// Gemini-specific request options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GeminiRequestOptions {
    #[serde(default)]
    pub thoughts: Option<bool>,
}

/// Provider-specific request options that should be forwarded on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProviderRequestOptions {
    #[serde(default)]
    pub tool_search_mode: ToolSearchMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningOptions>,
    #[serde(default)]
    pub responses: ResponsesRequestOptions,
    #[serde(default)]
    pub anthropic: AnthropicRequestOptions,
    #[serde(default)]
    pub gemini: GeminiRequestOptions,
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
    pub provider_request_options: ProviderRequestOptions,
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
            provider_request_options: self.provider_request_options,
        }
    }
}

/// Provider-neutral transcript item used for history compaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CompactionInputItem {
    UserTurn {
        content: String,
    },
    AssistantTurn {
        content: String,
    },
    ToolExchange {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request: Option<String>,
        result: String,
        is_error: bool,
    },
    CanonicalContext {
        content: String,
    },
    MemoryRecall {
        content: String,
    },
    DelegationResult {
        agent_id: String,
        agent_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<String>,
        status: String,
        content: String,
    },
    CompactionSummary {
        content: String,
    },
}

/// Provider-neutral request assembled for history compaction.
#[derive(Debug, Clone)]
pub struct CompactionRequest<'a> {
    pub model: Cow<'a, str>,
    pub instructions: Cow<'a, str>,
    pub input: Cow<'a, [CompactionInputItem]>,
    pub metadata: Cow<'a, BTreeMap<String, String>>,
    pub provider_request_options: ProviderRequestOptions,
}

impl CompactionRequest<'_> {
    pub fn into_owned(self) -> CompactionRequest<'static> {
        CompactionRequest {
            model: Cow::Owned(self.model.into_owned()),
            instructions: Cow::Owned(self.instructions.into_owned()),
            input: Cow::Owned(self.input.into_owned()),
            metadata: Cow::Owned(self.metadata.into_owned()),
            provider_request_options: self.provider_request_options,
        }
    }
}
