//! Request and response shapes for the OpenAI `chat/completions` wire.
//!
//! This is the wire almost every OpenAI-compatible endpoint speaks — DeepSeek,
//! Groq, Together, OpenRouter, vLLM, Ollama, LM Studio — and it is not the same
//! wire as `v1/responses`. The two differ in nearly every detail that matters:
//! a flat `messages` array rather than typed input items, tool results as their
//! own `role: "tool"` messages rather than blocks inside a user turn, tool
//! arguments as a JSON *string* rather than a value, and `max_tokens` rather
//! than `max_output_tokens`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use base64::{Engine as _, engine::general_purpose::STANDARD};

use crate::{
    ContentBlock, ImageSource, Message, ProviderError, ProviderToolKind, ReasoningEffort, Request,
    Role, ToolChoice, ToolResultContent, ToolSpec,
};

/// A `chat/completions` request body.
#[derive(Debug, Serialize)]
pub(crate) struct ChatCompletionsRequest {
    pub(crate) model: String,
    pub(crate) messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) tools: Vec<ChatTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_choice: Option<ChatToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) temperature: Option<f32>,
    #[serde(rename = "max_tokens", skip_serializing_if = "Option::is_none")]
    pub(crate) max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning_effort: Option<&'static str>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stream_options: Option<ChatStreamOptions>,
}

/// Asks for a final chunk carrying token usage.
///
/// Without it a streamed `chat/completions` turn reports no usage at all, and
/// every budget and usage report downstream sees zero. Endpoints that do not
/// know the field ignore it.
#[derive(Debug, Serialize)]
pub(crate) struct ChatStreamOptions {
    pub(crate) include_usage: bool,
}

#[derive(Debug, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub(crate) enum ChatMessage {
    System {
        content: String,
    },
    User {
        content: ChatUserContent,
    },
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ChatToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

/// A plain string where it can be one, an array of parts where it must be.
///
/// The string form is what every OpenAI-compatible endpoint accepts; the parts
/// form is only needed for images, and older or smaller gateways reject it for
/// text-only turns.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum ChatUserContent {
    Text(String),
    Parts(Vec<ChatContentPart>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ChatContentPart {
    Text { text: String },
    ImageUrl { image_url: ChatImageUrl },
}

#[derive(Debug, Serialize)]
pub(crate) struct ChatImageUrl {
    pub(crate) url: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct ChatToolCall {
    pub(crate) id: String,
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    pub(crate) function: ChatToolCallFunction,
}

#[derive(Debug, Serialize)]
pub(crate) struct ChatToolCallFunction {
    pub(crate) name: String,
    /// A JSON *string*, not a value. Every endpoint on this wire expects the
    /// arguments already serialized.
    pub(crate) arguments: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct ChatTool {
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    pub(crate) function: ChatToolFunction,
}

#[derive(Debug, Serialize)]
pub(crate) struct ChatToolFunction {
    pub(crate) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    pub(crate) parameters: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) strict: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum ChatToolChoice {
    Mode(&'static str),
    Function {
        #[serde(rename = "type")]
        kind: &'static str,
        function: ChatToolChoiceFunction,
    },
}

#[derive(Debug, Serialize)]
pub(crate) struct ChatToolChoiceFunction {
    pub(crate) name: String,
}

impl From<&ToolChoice> for ChatToolChoice {
    fn from(choice: &ToolChoice) -> Self {
        match choice {
            ToolChoice::Auto => Self::Mode("auto"),
            // `any` on other wires means "call some tool"; this wire spells it
            // `required`.
            ToolChoice::Any => Self::Mode("required"),
            ToolChoice::Tool { name } => Self::Function {
                kind: "function",
                function: ChatToolChoiceFunction { name: name.clone() },
            },
        }
    }
}

fn reasoning_effort_value(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        // This wire's vocabulary stops at `high`; asking for more asks for the
        // most it knows rather than a value it will reject.
        ReasoningEffort::XHigh | ReasoningEffort::Max => "high",
    }
}

impl ChatCompletionsRequest {
    pub(crate) fn from_request(request: Request<'_>, stream: bool) -> Result<Self, ProviderError> {
        let mut messages = Vec::new();
        if let Some(system) = request.system.as_deref() {
            messages.push(ChatMessage::System {
                content: system.to_string(),
            });
        }
        for message in request.messages.iter() {
            append_message(&mut messages, message)?;
        }

        let tools = request
            .tools
            .iter()
            .filter(|tool| matches!(tool.kind, ProviderToolKind::Function))
            .map(build_tool)
            .collect::<Vec<_>>();

        Ok(Self {
            model: request.model.into_owned(),
            messages,
            tools,
            tool_choice: request.tool_choice.as_ref().map(ChatToolChoice::from),
            temperature: request.temperature,
            max_output_tokens: request.max_output_tokens,
            reasoning_effort: request
                .provider_request_options
                .reasoning
                .as_ref()
                .and_then(|reasoning| reasoning.effort)
                .map(reasoning_effort_value),
            stream,
            stream_options: stream.then_some(ChatStreamOptions {
                include_usage: true,
            }),
        })
    }
}

fn build_tool(tool: &ToolSpec) -> ChatTool {
    ChatTool {
        kind: "function",
        function: ChatToolFunction {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.input_schema.clone(),
            strict: tool.strict,
        },
    }
}

/// Flattens one provider-neutral message into the messages this wire wants.
///
/// One message in can be several out: tool results live inside a user turn
/// everywhere else and are their own `role: "tool"` messages here, and they
/// must precede whatever else that turn carried.
fn append_message(messages: &mut Vec<ChatMessage>, message: &Message) -> Result<(), ProviderError> {
    for block in &message.content {
        if let ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } = block
        {
            messages.push(ChatMessage::Tool {
                tool_call_id: tool_use_id.clone(),
                content: tool_result_text(content)?,
            });
        }
    }

    match message.role {
        Role::Assistant => append_assistant_message(messages, message),
        _ => append_user_message(messages, message),
    }
}

fn append_assistant_message(
    messages: &mut Vec<ChatMessage>,
    message: &Message,
) -> Result<(), ProviderError> {
    let mut text = String::new();
    let mut tool_calls = Vec::new();

    for block in &message.content {
        match block {
            ContentBlock::Text { text: chunk } => text.push_str(chunk),
            ContentBlock::ToolUse { id, name, input } => tool_calls.push(ChatToolCall {
                id: id.clone(),
                kind: "function",
                function: ChatToolCallFunction {
                    name: name.clone(),
                    arguments: serde_json::to_string(input).map_err(ProviderError::Serialize)?,
                },
            }),
            // A model's own reasoning is not replayed on this wire. Providers
            // that emit `reasoning_content` document that it must not be sent
            // back, and the rest have nowhere to put it.
            ContentBlock::Thinking { .. } => {}
            // Already emitted as `role: "tool"` messages above.
            ContentBlock::ToolResult { .. } => {}
            ContentBlock::Image { .. }
            | ContentBlock::HostedToolSearch { .. }
            | ContentBlock::HostedWebSearch { .. }
            | ContentBlock::ImageGeneration { .. } => {}
        }
    }

    if text.is_empty() && tool_calls.is_empty() {
        return Ok(());
    }

    messages.push(ChatMessage::Assistant {
        content: (!text.is_empty()).then_some(text),
        tool_calls,
    });
    Ok(())
}

fn append_user_message(
    messages: &mut Vec<ChatMessage>,
    message: &Message,
) -> Result<(), ProviderError> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut has_image = false;

    for block in &message.content {
        match block {
            ContentBlock::Text { text: chunk } => {
                text.push_str(chunk);
                parts.push(ChatContentPart::Text {
                    text: chunk.clone(),
                });
            }
            ContentBlock::Image { source } => {
                has_image = true;
                parts.push(ChatContentPart::ImageUrl {
                    image_url: ChatImageUrl {
                        url: image_url(source),
                    },
                });
            }
            ContentBlock::ToolResult { .. } => {}
            ContentBlock::Thinking { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::HostedToolSearch { .. }
            | ContentBlock::HostedWebSearch { .. }
            | ContentBlock::ImageGeneration { .. } => {}
        }
    }

    if parts.is_empty() {
        return Ok(());
    }

    messages.push(ChatMessage::User {
        content: if has_image {
            ChatUserContent::Parts(parts)
        } else {
            ChatUserContent::Text(text)
        },
    });
    Ok(())
}

fn image_url(source: &ImageSource) -> String {
    match source {
        ImageSource::Url { url } => url.clone(),
        ImageSource::Bytes { media_type, data } => {
            format!("data:{media_type};base64,{}", STANDARD.encode(data))
        }
    }
}

fn tool_result_text(content: &ToolResultContent) -> Result<String, ProviderError> {
    match content {
        ToolResultContent::Text(text) => Ok(text.clone()),
        ToolResultContent::Structured(value) => {
            serde_json::to_string(value).map_err(ProviderError::Serialize)
        }
    }
}

/// One `chat.completion.chunk` from a streamed response.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ChatCompletionChunk {
    #[serde(default)]
    pub(crate) id: Option<String>,
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) choices: Vec<ChatChunkChoice>,
    #[serde(default)]
    pub(crate) usage: Option<ChatUsage>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ChatChunkChoice {
    #[serde(default)]
    pub(crate) delta: ChatDelta,
    #[serde(default)]
    pub(crate) finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ChatDelta {
    #[serde(default)]
    pub(crate) content: Option<String>,
    /// DeepSeek and vLLM spell separated reasoning `reasoning_content`; several
    /// gateways spell the same thing `reasoning`. Both are read.
    #[serde(default)]
    pub(crate) reasoning_content: Option<String>,
    #[serde(default)]
    pub(crate) reasoning: Option<String>,
    #[serde(default)]
    pub(crate) tool_calls: Vec<ChatToolCallDelta>,
}

impl ChatDelta {
    pub(crate) fn reasoning_text(&self) -> Option<&str> {
        self.reasoning_content
            .as_deref()
            .or(self.reasoning.as_deref())
            .filter(|text| !text.is_empty())
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatToolCallDelta {
    #[serde(default)]
    pub(crate) index: usize,
    #[serde(default)]
    pub(crate) id: Option<String>,
    #[serde(default)]
    pub(crate) function: Option<ChatFunctionDelta>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatFunctionDelta {
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatUsage {
    #[serde(default)]
    pub(crate) prompt_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) completion_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) total_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) prompt_tokens_details: Option<ChatPromptTokensDetails>,
    #[serde(default)]
    pub(crate) completion_tokens_details: Option<ChatCompletionTokensDetails>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatPromptTokensDetails {
    #[serde(default)]
    pub(crate) cached_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatCompletionTokensDetails {
    #[serde(default)]
    pub(crate) reasoning_tokens: Option<u64>,
}

/// The model list returned by `GET /v1/models`.
#[derive(Debug, Deserialize)]
pub(crate) struct ChatModelsPage {
    #[serde(default)]
    pub(crate) data: Vec<ChatModel>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatModel {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) created: Option<i64>,
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::collections::BTreeMap;

    use super::*;
    use crate::{ProviderRequestOptions, ReasoningOptions};

    fn request(messages: Vec<Message>, system: Option<&'static str>) -> Request<'static> {
        Request {
            model: Cow::Borrowed("a-model"),
            system: system.map(Cow::Borrowed),
            messages: Cow::Owned(messages),
            tools: Cow::Owned(vec![]),
            tool_choice: None,
            temperature: None,
            max_output_tokens: Some(256),
            metadata: Cow::Owned(BTreeMap::new()),
            provider_request_options: ProviderRequestOptions::default(),
        }
    }

    fn wire(request: Request<'_>) -> Value {
        serde_json::to_value(
            ChatCompletionsRequest::from_request(request, true).expect("request converts"),
        )
        .expect("request serializes")
    }

    #[test]
    fn a_tool_result_becomes_its_own_message_before_the_turn_that_carried_it() {
        // Every other wire nests a tool result inside the following user turn.
        // Here it is a message of its own, and it has to precede whatever else
        // that turn said or the transcript reads out of order.
        let body = wire(request(
            vec![
                Message::assistant(ContentBlock::ToolUse {
                    id: "call_a".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::json!({"path": "a.rs"}),
                }),
                Message {
                    role: Role::User,
                    content: vec![
                        ContentBlock::ToolResult {
                            tool_use_id: "call_a".to_string(),
                            content: ToolResultContent::text("fn main() {}"),
                            is_error: false,
                        },
                        ContentBlock::text("what does it do?"),
                    ],
                },
            ],
            Some("be brief"),
        ));

        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 4, "{body}");
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "be brief");

        assert_eq!(messages[1]["role"], "assistant");
        // Arguments are a JSON string on this wire, not a value.
        assert_eq!(
            messages[1]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a.rs\"}"
        );
        assert_eq!(messages[1]["tool_calls"][0]["id"], "call_a");
        assert_eq!(messages[1]["tool_calls"][0]["type"], "function");

        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call_a");
        assert_eq!(messages[2]["content"], "fn main() {}");

        assert_eq!(messages[3]["role"], "user");
        assert_eq!(messages[3]["content"], "what does it do?");
    }

    #[test]
    fn a_text_only_turn_sends_a_string_and_an_image_turn_sends_parts() {
        // The string form is what every endpoint on this wire accepts; the
        // parts form is only needed for images, and the smaller gateways reject
        // it for text.
        let text_only = wire(request(
            vec![Message::user(ContentBlock::text("hello"))],
            None,
        ));
        assert_eq!(text_only["messages"][0]["content"], "hello");

        let with_image = wire(request(
            vec![Message {
                role: Role::User,
                content: vec![
                    ContentBlock::text("what is this?"),
                    ContentBlock::Image {
                        source: ImageSource::bytes("image/png", vec![1, 2, 3]),
                    },
                ],
            }],
            None,
        ));
        let parts = with_image["messages"][0]["content"]
            .as_array()
            .expect("parts array");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "what is this?");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AQID");
    }

    #[test]
    fn the_budget_is_sent_as_max_tokens_with_usage_requested() {
        let body = wire(request(vec![Message::user(ContentBlock::text("hi"))], None));

        assert_eq!(body["max_tokens"], 256);
        assert!(body.get("max_output_tokens").is_none(), "{body}");
        // Without this a streamed turn reports no usage at all.
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn tool_choice_any_is_sent_as_required() {
        let mut request = request(vec![Message::user(ContentBlock::text("hi"))], None);
        request.tool_choice = Some(ToolChoice::Any);
        assert_eq!(wire(request)["tool_choice"], "required");

        let mut request = request_with_named_choice();
        request.tool_choice = Some(ToolChoice::Tool {
            name: "read_file".to_string(),
        });
        let body = wire(request);
        assert_eq!(body["tool_choice"]["type"], "function");
        assert_eq!(body["tool_choice"]["function"]["name"], "read_file");
    }

    fn request_with_named_choice() -> Request<'static> {
        request(vec![Message::user(ContentBlock::text("hi"))], None)
    }

    #[test]
    fn reasoning_beyond_this_wires_vocabulary_asks_for_the_most_it_knows() {
        let mut request = request(vec![Message::user(ContentBlock::text("hi"))], None);
        request.provider_request_options.reasoning = Some(ReasoningOptions {
            effort: Some(ReasoningEffort::Max),
            summary: None,
        });

        assert_eq!(wire(request)["reasoning_effort"], "high");
    }

    #[test]
    fn a_models_own_reasoning_is_not_replayed_to_it() {
        // Providers that separate reasoning document that it must not be sent
        // back, and the rest of this wire has nowhere to put it.
        let body = wire(request(
            vec![Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::thinking("scratch work"),
                    ContentBlock::text("the answer"),
                ],
            }],
            None,
        ));

        assert_eq!(body["messages"][0]["content"], "the answer");
        let serialized = body.to_string();
        assert!(!serialized.contains("scratch work"), "{serialized}");
    }
}
