use serde::Deserialize;
use serde::Serialize;
use std::borrow::Cow;
use std::collections::BTreeMap;

use crate::ContentBlock;
use crate::Message;
use crate::ProviderError;
use crate::model::ToolChoice;
use crate::tool::ToolSpec;

/// Provider-neutral reasoning controls supported across multiple providers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<ReasoningSummary>,
}

/// Shared reasoning effort levels supported by Mentra's public API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

/// Shared reasoning summary levels used by Responses-family providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningSummary {
    Auto,
    Concise,
    Detailed,
}

/// Provider-neutral tool search behavior requested for a model call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ToolSearchMode {
    #[default]
    Disabled,
    Hosted,
}

/// Responses-compatible verbosity controls for text output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ResponsesVerbosity {
    Low,
    #[default]
    Medium,
    High,
}

/// Transport-level request compression supported by Responses-family HTTP calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ResponsesRequestCompression {
    #[default]
    None,
    Zstd,
}

/// Provider-side conversation state strategy for Responses-family providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResponsesStateMode {
    /// Send the complete local transcript and do not attach provider-side state.
    ReplayOnly,
    /// Keep local replay as the source of truth while opportunistically chaining provider state.
    #[default]
    Hybrid,
    /// Require provider-side state chaining once a previous response id is available.
    Stateful,
}

impl ResponsesStateMode {
    pub fn uses_provider_state(self) -> bool {
        matches!(self, Self::Hybrid | Self::Stateful)
    }
}

/// Responses-compatible format discriminator for structured text output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResponsesTextFormatType {
    #[default]
    JsonSchema,
}

/// Structured text output format controls for Responses-family providers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ResponsesTextFormat {
    #[serde(default)]
    pub r#type: ResponsesTextFormatType,
    #[serde(default)]
    pub strict: bool,
    pub schema: serde_json::Value,
    pub name: String,
}

/// Responses-compatible text controls combining verbosity and output schemas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ResponsesTextControls {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<ResponsesVerbosity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<ResponsesTextFormat>,
}

/// Shared Responses-family request options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponsesRequestOptions {
    #[serde(default)]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    #[serde(default)]
    pub state_mode: ResponsesStateMode,
    #[serde(default)]
    pub store: Option<bool>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<ResponsesTextControls>,
    #[serde(default)]
    pub compression: ResponsesRequestCompression,
}

impl Default for ResponsesRequestOptions {
    fn default() -> Self {
        Self {
            parallel_tool_calls: None,
            previous_response_id: None,
            state_mode: ResponsesStateMode::Hybrid,
            store: None,
            stream: Some(true),
            include: Vec::new(),
            service_tier: None,
            prompt_cache_key: None,
            text: None,
            compression: ResponsesRequestCompression::None,
        }
    }
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

/// Provider-neutral session metadata and affinity hints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SessionRequestOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sticky_turn_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_metadata: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefer_connection_reuse: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_affinity: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_headers: BTreeMap<String, String>,
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
    #[serde(default)]
    pub session: SessionRequestOptions,
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
    /// Converts a compaction request into an ordinary model request.
    pub fn into_model_request(self) -> Result<Request<'static>, ProviderError> {
        let input_json =
            serde_json::to_string(self.input.as_ref()).map_err(ProviderError::Serialize)?;

        Ok(Request {
            model: Cow::Owned(self.model.into_owned()),
            system: Some(Cow::Owned(self.instructions.into_owned())),
            messages: Cow::Owned(vec![Message::user(ContentBlock::text(format!(
                "Compaction input JSON:\n{input_json}"
            )))]),
            tools: Cow::Owned(Vec::new()),
            tool_choice: None,
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(self.metadata.into_owned()),
            provider_request_options: self.provider_request_options,
        })
    }
}

/// Canonical raw memory payload used by memory summarization requests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawMemory {
    pub id: String,
    pub metadata: RawMemoryMetadata,
    pub items: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawMemoryMetadata {
    pub source_path: String,
}

/// Provider-neutral request assembled for trace memory summarization.
#[derive(Debug, Clone)]
pub struct MemorySummarizeRequest<'a> {
    pub model: Cow<'a, str>,
    pub raw_memories: Cow<'a, [RawMemory]>,
    pub reasoning: Option<ReasoningOptions>,
    pub metadata: Cow<'a, BTreeMap<String, String>>,
    pub provider_request_options: ProviderRequestOptions,
}

impl MemorySummarizeRequest<'_> {
    /// Converts a memory summarize request into an ordinary model request.
    pub fn into_model_request(self) -> Result<Request<'static>, ProviderError> {
        let raw_memories_json =
            serde_json::to_string(self.raw_memories.as_ref()).map_err(ProviderError::Serialize)?;

        Ok(Request {
            model: Cow::Owned(self.model.into_owned()),
            system: Some(Cow::Borrowed(MEMORY_SUMMARIZE_SYSTEM_PROMPT)),
            messages: Cow::Owned(vec![Message::user(ContentBlock::text(format!(
                "Memory summarize input JSON:\n{raw_memories_json}"
            )))]),
            tools: Cow::Owned(Vec::new()),
            tool_choice: None,
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(self.metadata.into_owned()),
            provider_request_options: ProviderRequestOptions {
                reasoning: self.reasoning,
                ..self.provider_request_options
            },
        })
    }
}

const MEMORY_SUMMARIZE_SYSTEM_PROMPT: &str = concat!(
    "You summarize trace memories for Codex.\n",
    "Return valid JSON only.\n",
    "The output must be a JSON array with one object per input trace, in the same order.\n",
    "Each object must have exactly these string fields: `raw_memory` and `memory_summary`.\n",
    "`raw_memory` should be a concrete, detailed summary of the trace contents.\n",
    "`memory_summary` should be a shorter durable takeaway focused on reusable context.\n",
    "Use empty strings when information is unavailable.\n",
    "Do not include markdown fences or extra commentary.\n",
);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn compaction_request_into_model_request_serializes_input_as_prompt_text() {
        let request = CompactionRequest {
            model: Cow::Borrowed("gpt-5"),
            instructions: Cow::Borrowed("Summarize the transcript."),
            input: Cow::Owned(vec![
                CompactionInputItem::UserTurn {
                    content: "hello".to_string(),
                },
                CompactionInputItem::AssistantTurn {
                    content: "world".to_string(),
                },
            ]),
            metadata: Cow::Owned(BTreeMap::from([("scope".to_string(), "test".to_string())])),
            provider_request_options: ProviderRequestOptions {
                session: SessionRequestOptions {
                    sticky_turn_state: Some("sticky".to_string()),
                    turn_metadata: None,
                    subagent: Some("compact".to_string()),
                    prefer_connection_reuse: Some(true),
                    session_affinity: None,
                    extra_headers: BTreeMap::new(),
                },
                ..ProviderRequestOptions::default()
            },
        };

        let model_request = request
            .into_model_request()
            .expect("compaction request should convert");

        assert_eq!(model_request.model.as_ref(), "gpt-5");
        assert_eq!(
            model_request.system.as_deref(),
            Some("Summarize the transcript.")
        );
        assert_eq!(model_request.metadata["scope"], "test");
        assert_eq!(
            model_request
                .provider_request_options
                .session
                .sticky_turn_state
                .as_deref(),
            Some("sticky")
        );
        assert_eq!(
            model_request
                .provider_request_options
                .session
                .subagent
                .as_deref(),
            Some("compact")
        );
        assert_eq!(model_request.messages.len(), 1);

        let prompt = model_request.messages[0].text();
        assert!(prompt.starts_with("Compaction input JSON:\n"));
        let payload = prompt
            .strip_prefix("Compaction input JSON:\n")
            .expect("prompt should contain the compaction prefix");
        let input: Vec<Value> = serde_json::from_str(payload).expect("prompt should be json");
        assert_eq!(input[0]["type"], "user_turn");
        assert_eq!(input[0]["content"], "hello");
        assert_eq!(input[1]["type"], "assistant_turn");
        assert_eq!(input[1]["content"], "world");
    }

    #[test]
    fn memory_summarize_request_into_model_request_serializes_input_as_prompt_text() {
        let request = MemorySummarizeRequest {
            model: Cow::Borrowed("gpt-5"),
            raw_memories: Cow::Owned(vec![RawMemory {
                id: "memory-1".to_string(),
                metadata: RawMemoryMetadata {
                    source_path: "/tmp/trace.jsonl".to_string(),
                },
                items: vec![serde_json::json!({"type":"message","role":"user"})],
            }]),
            reasoning: Some(ReasoningOptions {
                effort: Some(ReasoningEffort::Medium),
                summary: None,
            }),
            metadata: Cow::Owned(BTreeMap::from([("scope".to_string(), "test".to_string())])),
            provider_request_options: ProviderRequestOptions {
                session: SessionRequestOptions {
                    sticky_turn_state: None,
                    turn_metadata: Some("{\"turn_id\":\"t1\"}".to_string()),
                    subagent: None,
                    prefer_connection_reuse: Some(true),
                    session_affinity: Some("thread-1".to_string()),
                    extra_headers: BTreeMap::new(),
                },
                ..ProviderRequestOptions::default()
            },
        };

        let model_request = request
            .into_model_request()
            .expect("memory summarize request should convert");

        assert_eq!(model_request.model.as_ref(), "gpt-5");
        assert_eq!(
            model_request.system.as_deref(),
            Some(MEMORY_SUMMARIZE_SYSTEM_PROMPT)
        );
        assert_eq!(model_request.metadata["scope"], "test");
        assert_eq!(
            model_request
                .provider_request_options
                .session
                .turn_metadata
                .as_deref(),
            Some("{\"turn_id\":\"t1\"}")
        );
        assert_eq!(
            model_request
                .provider_request_options
                .reasoning
                .as_ref()
                .expect("reasoning options")
                .effort,
            Some(ReasoningEffort::Medium)
        );
        assert_eq!(model_request.messages.len(), 1);

        let prompt = model_request.messages[0].text();
        assert!(prompt.starts_with("Memory summarize input JSON:\n"));
        let payload = prompt
            .strip_prefix("Memory summarize input JSON:\n")
            .expect("prompt should contain the memory summarize prefix");
        let input: Vec<RawMemory> = serde_json::from_str(payload).expect("prompt should be json");
        assert_eq!(input[0].id, "memory-1");
        assert_eq!(input[0].metadata.source_path, "/tmp/trace.jsonl");
        assert_eq!(input[0].items[0]["role"], "user");
    }
}
