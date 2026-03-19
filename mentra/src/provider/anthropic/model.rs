use std::borrow::Cow;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    provider::model::{
        BuiltinProvider, ContentBlock, ImageSource, Message, ModelInfo, ProviderError, Request,
        Response, Role, TokenUsage, ToolChoice, ToolSearchMode,
    },
    tool::{ToolLoadingPolicy, ToolSpec},
};

#[derive(Deserialize)]
pub(crate) struct AnthropicModelsPage {
    pub(crate) data: Vec<AnthropicModel>,
    pub(crate) has_more: bool,
    pub(crate) last_id: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct AnthropicModel {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) display_name: Option<String>,
    #[serde(default)]
    pub(crate) created_at: Option<String>,
}

impl From<AnthropicModel> for ModelInfo {
    fn from(model: AnthropicModel) -> Self {
        ModelInfo {
            id: model.id,
            provider: BuiltinProvider::Anthropic.into(),
            display_name: model.display_name,
            description: None,
            created_at: model
                .created_at
                .as_deref()
                .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok()),
        }
    }
}

#[derive(Serialize)]
pub(crate) struct AnthropicRequest {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<AnthropicToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(rename = "max_tokens", skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disable_parallel_tool_use: Option<bool>,
}

#[derive(Deserialize)]
pub(crate) struct AnthropicResponse {
    pub(crate) id: String,
    pub(crate) model: String,
    pub(crate) role: String,
    #[serde(default)]
    pub(crate) usage: Option<AnthropicUsage>,
    content: Vec<AnthropicContentBlock>,
    stop_reason: Option<String>,
}

impl TryFrom<AnthropicResponse> for Response {
    type Error = ProviderError;

    fn try_from(response: AnthropicResponse) -> Result<Self, Self::Error> {
        Ok(Response {
            id: response.id,
            model: response.model,
            role: match response.role.as_str() {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                _ => Role::Unknown(response.role),
            },
            content: response
                .content
                .into_iter()
                .map(ContentBlock::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            stop_reason: response.stop_reason,
            usage: response.usage.and_then(|usage| usage.into_token_usage()),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AnthropicUsage {
    #[serde(default)]
    pub(crate) input_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) output_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) total_tokens: Option<u64>,
}

impl AnthropicUsage {
    pub(crate) fn into_token_usage(self) -> Option<TokenUsage> {
        let usage = TokenUsage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            total_tokens: self.total_tokens,
            cache_read_input_tokens: self.cache_read_input_tokens,
            cache_creation_input_tokens: self.cache_creation_input_tokens,
            reasoning_tokens: None,
            thoughts_tokens: None,
            tool_input_tokens: None,
        };

        (!usage.is_empty()).then_some(usage)
    }
}

impl<'a> TryFrom<Request<'a>> for AnthropicRequest {
    type Error = ProviderError;

    fn try_from(value: Request<'a>) -> Result<Self, Self::Error> {
        Ok(AnthropicRequest {
            model: value.model.into_owned(),
            system: value.system.map(Cow::into_owned),
            messages: value
                .messages
                .iter()
                .map(AnthropicMessage::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            tools: build_anthropic_tools(
                value.tools.as_ref(),
                value.tool_choice.as_ref(),
                value.provider_request_options.tool_search_mode,
            )?,
            tool_choice: value.tool_choice.map(|choice| choice.into()),
            temperature: value.temperature,
            max_output_tokens: value.max_output_tokens,
            disable_parallel_tool_use: value
                .provider_request_options
                .anthropic
                .disable_parallel_tool_use,
        })
    }
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: String,
    content: Vec<AnthropicContentBlock>,
}

impl TryFrom<Message> for AnthropicMessage {
    type Error = ProviderError;

    fn try_from(message: Message) -> Result<Self, Self::Error> {
        AnthropicMessage::try_from(&message)
    }
}

impl TryFrom<&Message> for AnthropicMessage {
    type Error = ProviderError;

    fn try_from(message: &Message) -> Result<Self, Self::Error> {
        if !matches!(message.role, Role::User) && message_has_image(message) {
            return Err(ProviderError::InvalidRequest(
                "Anthropic image inputs are only supported in user messages".to_string(),
            ));
        }

        Ok(AnthropicMessage {
            role: message.role.to_string(),
            content: message.content.iter().map(|block| block.into()).collect(),
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlock {
    Text {
        text: String,
    },
    Image {
        source: AnthropicImageSource,
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

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicImageSource {
    Base64 { media_type: String, data: String },
    Url { url: String },
}

impl From<ContentBlock> for AnthropicContentBlock {
    fn from(block: ContentBlock) -> Self {
        AnthropicContentBlock::from(&block)
    }
}

impl From<&ContentBlock> for AnthropicContentBlock {
    fn from(block: &ContentBlock) -> Self {
        match block {
            ContentBlock::Text { text } => AnthropicContentBlock::Text { text: text.clone() },
            ContentBlock::Image { source } => AnthropicContentBlock::Image {
                source: source.into(),
            },
            ContentBlock::ToolUse { id, name, input } => AnthropicContentBlock::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            },
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => AnthropicContentBlock::ToolResult {
                tool_use_id: tool_use_id.clone(),
                content: content.clone(),
                is_error: *is_error,
            },
        }
    }
}

impl TryFrom<AnthropicContentBlock> for ContentBlock {
    type Error = ProviderError;

    fn try_from(block: AnthropicContentBlock) -> Result<Self, Self::Error> {
        Ok(match block {
            AnthropicContentBlock::Text { text } => ContentBlock::Text { text },
            AnthropicContentBlock::Image { source } => ContentBlock::Image {
                source: source.try_into()?,
            },
            AnthropicContentBlock::ToolUse { id, name, input } => {
                ContentBlock::ToolUse { id, name, input }
            }
            AnthropicContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            },
        })
    }
}

impl From<&ImageSource> for AnthropicImageSource {
    fn from(value: &ImageSource) -> Self {
        match value {
            ImageSource::Bytes { media_type, data } => AnthropicImageSource::Base64 {
                media_type: media_type.clone(),
                data: STANDARD.encode(data),
            },
            ImageSource::Url { url } => AnthropicImageSource::Url { url: url.clone() },
        }
    }
}

impl From<ImageSource> for AnthropicImageSource {
    fn from(value: ImageSource) -> Self {
        AnthropicImageSource::from(&value)
    }
}

impl TryFrom<AnthropicImageSource> for ImageSource {
    type Error = ProviderError;

    fn try_from(value: AnthropicImageSource) -> Result<Self, Self::Error> {
        match value {
            AnthropicImageSource::Base64 { media_type, data } => {
                let data = STANDARD.decode(data).map_err(|error| {
                    ProviderError::InvalidResponse(format!(
                        "invalid Anthropic image payload for media type {media_type}: {error}"
                    ))
                })?;
                Ok(ImageSource::Bytes { media_type, data })
            }
            AnthropicImageSource::Url { url } => Ok(ImageSource::Url { url }),
        }
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum AnthropicTool {
    Custom(AnthropicCustomTool),
    HostedSearch(AnthropicHostedSearchTool),
}

#[derive(Serialize)]
struct AnthropicCustomTool {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    input_schema: Value,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    defer_loading: bool,
}

#[derive(Serialize)]
struct AnthropicHostedSearchTool {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'static str,
}

impl AnthropicTool {
    fn custom(tool: &ToolSpec, force_immediate: bool) -> Self {
        Self::Custom(AnthropicCustomTool {
            name: tool.name.clone(),
            description: tool.description.clone(),
            input_schema: tool.input_schema.clone(),
            defer_loading: tool.loading_policy == ToolLoadingPolicy::Deferred && !force_immediate,
        })
    }

    fn hosted_search() -> Self {
        Self::HostedSearch(AnthropicHostedSearchTool {
            kind: "tool_search_tool_bm25_20251119",
            name: "tool_search_tool_bm25",
        })
    }
}

fn build_anthropic_tools(
    tools: &[ToolSpec],
    tool_choice: Option<&ToolChoice>,
    tool_search_mode: ToolSearchMode,
) -> Result<Vec<AnthropicTool>, ProviderError> {
    let forced_tool_name = match tool_choice {
        Some(ToolChoice::Tool { name }) => Some(name.as_str()),
        _ => None,
    };

    let has_deferred_tools = tools.iter().any(|tool| {
        tool.loading_policy == ToolLoadingPolicy::Deferred
            && forced_tool_name != Some(tool.name.as_str())
    });

    if has_deferred_tools && tool_search_mode != ToolSearchMode::Hosted {
        return Err(ProviderError::InvalidRequest(
            "Anthropic deferred tools require hosted tool search".to_string(),
        ));
    }

    let mut provider_tools = tools
        .iter()
        .map(|tool| AnthropicTool::custom(tool, forced_tool_name == Some(tool.name.as_str())))
        .collect::<Vec<_>>();

    if has_deferred_tools {
        provider_tools.push(AnthropicTool::hosted_search());
    }

    Ok(provider_tools)
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum AnthropicToolChoice {
    Auto,
    Any,
    Tool { name: String },
}

impl From<ToolChoice> for AnthropicToolChoice {
    fn from(choice: ToolChoice) -> Self {
        match choice {
            ToolChoice::Auto => AnthropicToolChoice::Auto,
            ToolChoice::Any => AnthropicToolChoice::Any,
            ToolChoice::Tool { name } => AnthropicToolChoice::Tool { name },
        }
    }
}

fn message_has_image(message: &Message) -> bool {
    message
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::Image { .. }))
}

#[cfg(test)]
mod tests {
    use std::{borrow::Cow, collections::BTreeMap};

    use time::{OffsetDateTime, format_description::well_known::Rfc3339};

    use crate::provider::model::{
        AnthropicRequestOptions, ContentBlock, Message, ProviderError, ProviderRequestOptions,
        Request, Role, ToolChoice, ToolSearchMode,
    };
    use crate::tool::{ToolLoadingPolicy, ToolSpec};

    use super::{AnthropicContentBlock, AnthropicImageSource, AnthropicModel, AnthropicRequest};

    #[test]
    fn converts_rfc3339_timestamp_to_offset_datetime() {
        let raw = "2025-03-04T12:34:56Z";
        let model = AnthropicModel {
            id: "claude-test".to_string(),
            display_name: None,
            created_at: Some(raw.to_string()),
        };

        let info = crate::provider::model::ModelInfo::from(model);

        assert_eq!(
            info.created_at,
            Some(OffsetDateTime::parse(raw, &Rfc3339).expect("valid rfc3339"))
        );
    }

    #[test]
    fn serializes_inline_images_into_anthropic_content_blocks() {
        let request = Request {
            model: Cow::Borrowed("claude-sonnet"),
            system: None,
            messages: Cow::Owned(vec![Message {
                role: Role::User,
                content: vec![
                    ContentBlock::text("Describe this"),
                    ContentBlock::image_bytes("image/png", [1_u8, 2, 3]),
                    ContentBlock::ToolResult {
                        tool_use_id: "call_1".to_string(),
                        content: "ok".to_string(),
                        is_error: false,
                    },
                ],
            }]),
            tools: Cow::Owned(vec![]),
            tool_choice: Some(ToolChoice::Auto),
            temperature: Some(0.1),
            max_output_tokens: Some(512),
            metadata: Cow::Owned(BTreeMap::new()),
            provider_request_options: ProviderRequestOptions::default(),
        };

        let payload = serde_json::to_value(AnthropicRequest::try_from(request).unwrap())
            .expect("request should serialize");

        assert_eq!(payload["messages"][0]["role"], "user");
        assert_eq!(payload["messages"][0]["content"][0]["type"], "text");
        assert_eq!(
            payload["messages"][0]["content"][0]["text"],
            "Describe this"
        );
        assert_eq!(payload["messages"][0]["content"][1]["type"], "image");
        assert_eq!(
            payload["messages"][0]["content"][1]["source"]["type"],
            "base64"
        );
        assert_eq!(
            payload["messages"][0]["content"][1]["source"]["media_type"],
            "image/png"
        );
        assert_eq!(
            payload["messages"][0]["content"][1]["source"]["data"],
            "AQID"
        );
        assert_eq!(payload["messages"][0]["content"][2]["type"], "tool_result");
        assert_eq!(payload["max_tokens"], 512);
        let temperature = payload["temperature"]
            .as_f64()
            .expect("temperature should be numeric");
        assert!((temperature - 0.1).abs() < 1e-6);
    }

    #[test]
    fn rejects_invalid_base64_image_payloads() {
        let error = ContentBlock::try_from(AnthropicContentBlock::Image {
            source: AnthropicImageSource::Base64 {
                media_type: "image/png".to_string(),
                data: "!not-base64!".to_string(),
            },
        })
        .expect_err("invalid base64 should fail");

        match error {
            ProviderError::InvalidResponse(message) => {
                assert!(message.contains("invalid Anthropic image payload"));
                assert!(message.contains("image/png"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn serializes_disable_parallel_tool_use_option() {
        let request = Request {
            model: Cow::Borrowed("claude-sonnet"),
            system: None,
            messages: Cow::Owned(vec![]),
            tools: Cow::Owned(vec![]),
            tool_choice: Some(ToolChoice::Auto),
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
            provider_request_options: ProviderRequestOptions {
                tool_search_mode: crate::provider::ToolSearchMode::Disabled,
                openai: Default::default(),
                anthropic: AnthropicRequestOptions {
                    disable_parallel_tool_use: Some(true),
                },
            },
        };

        let payload = serde_json::to_value(AnthropicRequest::try_from(request).unwrap())
            .expect("request should serialize");

        assert_eq!(payload["disable_parallel_tool_use"], true);
    }

    #[test]
    fn hosted_tool_search_adds_search_tool_for_deferred_tools() {
        let request = Request {
            model: Cow::Borrowed("claude-sonnet"),
            system: None,
            messages: Cow::Owned(vec![Message::user(ContentBlock::text("hello"))]),
            tools: Cow::Owned(vec![ToolSpec {
                name: "lookup_order".to_string(),
                description: Some("Look up an order".to_string()),
                input_schema: serde_json::json!({"type":"object"}),
                capabilities: vec![],
                side_effect_level: crate::tool::ToolSideEffectLevel::None,
                durability: crate::tool::ToolDurability::ReplaySafe,
                loading_policy: ToolLoadingPolicy::Deferred,
                execution_timeout: None,
            }]),
            tool_choice: Some(ToolChoice::Auto),
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
            provider_request_options: ProviderRequestOptions {
                tool_search_mode: ToolSearchMode::Hosted,
                ..Default::default()
            },
        };

        let payload = serde_json::to_value(AnthropicRequest::try_from(request).unwrap())
            .expect("request should serialize");

        assert_eq!(payload["tools"][0]["name"], "lookup_order");
        assert_eq!(payload["tools"][0]["defer_loading"], true);
        assert_eq!(payload["tools"][1]["type"], "tool_search_tool_bm25_20251119");
        assert_eq!(payload["tools"][1]["name"], "tool_search_tool_bm25");
    }

    #[test]
    fn rejects_deferred_tools_without_hosted_tool_search() {
        let request = Request {
            model: Cow::Borrowed("claude-sonnet"),
            system: None,
            messages: Cow::Owned(vec![]),
            tools: Cow::Owned(vec![ToolSpec {
                name: "lookup_order".to_string(),
                description: None,
                input_schema: serde_json::json!({"type":"object"}),
                capabilities: vec![],
                side_effect_level: crate::tool::ToolSideEffectLevel::None,
                durability: crate::tool::ToolDurability::ReplaySafe,
                loading_policy: ToolLoadingPolicy::Deferred,
                execution_timeout: None,
            }]),
            tool_choice: Some(ToolChoice::Auto),
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
            provider_request_options: ProviderRequestOptions::default(),
        };

        let error = AnthropicRequest::try_from(request)
            .err()
            .expect("request should fail");
        match error {
            ProviderError::InvalidRequest(message) => {
                assert!(message.contains("deferred tools require hosted tool search"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn forced_deferred_tool_serializes_as_immediate() {
        let request = Request {
            model: Cow::Borrowed("claude-sonnet"),
            system: None,
            messages: Cow::Owned(vec![]),
            tools: Cow::Owned(vec![ToolSpec {
                name: "lookup_order".to_string(),
                description: Some("Look up an order".to_string()),
                input_schema: serde_json::json!({"type":"object"}),
                capabilities: vec![],
                side_effect_level: crate::tool::ToolSideEffectLevel::None,
                durability: crate::tool::ToolDurability::ReplaySafe,
                loading_policy: ToolLoadingPolicy::Deferred,
                execution_timeout: None,
            }]),
            tool_choice: Some(ToolChoice::Tool {
                name: "lookup_order".to_string(),
            }),
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
            provider_request_options: ProviderRequestOptions::default(),
        };

        let payload = serde_json::to_value(AnthropicRequest::try_from(request).unwrap())
            .expect("request should serialize");

        assert_eq!(payload["tools"][0]["name"], "lookup_order");
        assert!(payload["tools"][0].get("defer_loading").is_none());
        assert!(payload["tools"].get(1).is_none());
        assert_eq!(payload["tool_choice"]["name"], "lookup_order");
    }
}
