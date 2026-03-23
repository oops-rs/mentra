use std::{borrow::Cow, collections::BTreeMap};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{
    BuiltinProvider,
    provider::model::{
        ContentBlock, ImageSource, Message, ModelInfo, ProviderError, Request, Role, ToolChoice,
        ToolSearchMode,
    },
    tool::{ToolLoadingPolicy, ToolSpec},
};

#[derive(Deserialize)]
pub(crate) struct OpenAIModelsPage {
    pub(crate) data: Vec<OpenAIModel>,
}

impl OpenAIModelsPage {
    pub(crate) fn into_model_info(self, provider: BuiltinProvider) -> Vec<ModelInfo> {
        self.data
            .into_iter()
            .map(|model| model.into_model_info(provider))
            .collect()
    }
}

#[derive(Deserialize)]
pub(crate) struct OpenAIModel {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) owned_by: Option<String>,
    #[serde(default)]
    pub(crate) created: Option<u64>,
}

impl OpenAIModel {
    pub(crate) fn into_model_info(self, provider: BuiltinProvider) -> ModelInfo {
        ModelInfo {
            id: self.id,
            provider: provider.into(),
            display_name: self.name,
            description: self
                .description
                .or_else(|| self.owned_by.map(|owner| format!("Owned by {owner}"))),
            created_at: self
                .created
                .and_then(|timestamp| OffsetDateTime::from_unix_timestamp(timestamp as i64).ok()),
        }
    }
}

impl From<OpenAIModel> for ModelInfo {
    fn from(model: OpenAIModel) -> Self {
        model.into_model_info(BuiltinProvider::OpenAI)
    }
}

#[derive(Serialize)]
pub(crate) struct OpenAIResponsesRequest {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    input: Vec<OpenAIInputItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<OpenAITool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<OpenAIToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    metadata: BTreeMap<String, String>,
}

impl<'a> TryFrom<Request<'a>> for OpenAIResponsesRequest {
    type Error = ProviderError;

    fn try_from(value: Request<'a>) -> Result<Self, Self::Error> {
        Self::try_from_request(value, "OpenAI")
    }
}

impl OpenAIResponsesRequest {
    pub(crate) fn try_from_request<'a>(
        value: Request<'a>,
        provider_name: &'static str,
    ) -> Result<Self, ProviderError> {
        let mut input = Vec::new();

        for message in value.messages.iter() {
            input.extend(OpenAIInputItem::from_message(message)?);
        }

        Ok(OpenAIResponsesRequest {
            model: value.model.into_owned(),
            instructions: value.system.map(Cow::into_owned),
            input,
            tools: build_openai_tools(
                value.tools.as_ref(),
                value.tool_choice.as_ref(),
                value.provider_request_options.tool_search_mode,
                provider_name,
            )?,
            tool_choice: value.tool_choice.map(Into::into),
            temperature: value.temperature,
            max_output_tokens: value.max_output_tokens,
            parallel_tool_calls: value.provider_request_options.openai.parallel_tool_calls,
            metadata: value.metadata.into_owned(),
        })
    }
}

#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum OpenAIInputItem {
    Message(OpenAIMessageInput),
    FunctionCall(OpenAIFunctionCallInput),
    FunctionCallOutput(OpenAIFunctionCallOutputInput),
}

impl OpenAIInputItem {
    fn from_message(message: &Message) -> Result<Vec<Self>, ProviderError> {
        let mut items = Vec::new();
        let mut content = Vec::new();
        let mut text_buffer = String::new();

        for block in &message.content {
            match block {
                ContentBlock::Text { text } => text_buffer.push_str(text),
                ContentBlock::Image { source } => {
                    Self::flush_text(&mut text_buffer, &message.role, &mut content)?;
                    content.push(OpenAIMessageContentPart::try_from((source, &message.role))?);
                }
                ContentBlock::ToolUse { id, name, input } => {
                    Self::flush_message(message, &mut text_buffer, &mut content, &mut items)?;
                    items.push(OpenAIInputItem::FunctionCall(OpenAIFunctionCallInput {
                        kind: "function_call",
                        call_id: id.clone(),
                        name: name.clone(),
                        arguments: input.to_string(),
                    }));
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content: tool_output,
                    is_error,
                } => {
                    Self::flush_message(message, &mut text_buffer, &mut content, &mut items)?;
                    items.push(OpenAIInputItem::FunctionCallOutput(
                        OpenAIFunctionCallOutputInput {
                            kind: "function_call_output",
                            call_id: tool_use_id.clone(),
                            output: render_tool_output(tool_output, *is_error),
                        },
                    ));
                }
            }
        }

        Self::flush_message(message, &mut text_buffer, &mut content, &mut items)?;
        Ok(items)
    }

    fn flush_text(
        text_buffer: &mut String,
        role: &Role,
        content: &mut Vec<OpenAIMessageContentPart>,
    ) -> Result<(), ProviderError> {
        if text_buffer.is_empty() {
            return Ok(());
        }

        content.push(OpenAIMessageContentPart::text_for_role(
            role,
            std::mem::take(text_buffer),
        )?);
        Ok(())
    }

    fn flush_message(
        message: &Message,
        text_buffer: &mut String,
        content: &mut Vec<OpenAIMessageContentPart>,
        items: &mut Vec<Self>,
    ) -> Result<(), ProviderError> {
        Self::flush_text(text_buffer, &message.role, content)?;
        if content.is_empty() {
            return Ok(());
        }

        items.push(OpenAIInputItem::Message(OpenAIMessageInput {
            role: message.role.to_string(),
            content: std::mem::take(content),
        }));
        Ok(())
    }
}

fn render_tool_output(content: &str, is_error: bool) -> String {
    if is_error {
        format!("Tool error: {content}")
    } else {
        content.to_string()
    }
}

#[derive(Serialize)]
pub(crate) struct OpenAIMessageInput {
    role: String,
    content: Vec<OpenAIMessageContentPart>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum OpenAIMessageContentPart {
    InputText { text: String },
    OutputText { text: String },
    InputImage { image_url: String },
}

impl OpenAIMessageContentPart {
    fn text_for_role(role: &Role, text: String) -> Result<Self, ProviderError> {
        match role {
            Role::User => Ok(Self::InputText { text }),
            Role::Assistant => Ok(Self::OutputText { text }),
            Role::Unknown(role) => Err(ProviderError::InvalidRequest(format!(
                "OpenAI message role '{role}' is not supported for text content"
            ))),
        }
    }
}

impl TryFrom<(&ImageSource, &Role)> for OpenAIMessageContentPart {
    type Error = ProviderError;

    fn try_from(value: (&ImageSource, &Role)) -> Result<Self, Self::Error> {
        let (source, role) = value;
        if !matches!(role, Role::User) {
            return Err(ProviderError::InvalidRequest(
                "OpenAI image inputs are only supported in user messages".to_string(),
            ));
        }

        let image_url = match source {
            ImageSource::Bytes { media_type, data } => {
                format!("data:{media_type};base64,{}", STANDARD.encode(data))
            }
            ImageSource::Url { url } => url.clone(),
        };

        Ok(OpenAIMessageContentPart::InputImage { image_url })
    }
}

#[derive(Serialize)]
pub(crate) struct OpenAIFunctionCallInput {
    #[serde(rename = "type")]
    kind: &'static str,
    call_id: String,
    name: String,
    arguments: String,
}

#[derive(Serialize)]
pub(crate) struct OpenAIFunctionCallOutputInput {
    #[serde(rename = "type")]
    kind: &'static str,
    call_id: String,
    output: String,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum OpenAITool {
    Function {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        parameters: serde_json::Value,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        defer_loading: bool,
    },
    ToolSearch {},
}

impl OpenAITool {
    fn function(tool: &ToolSpec, force_immediate: bool) -> Self {
        Self::Function {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.input_schema.clone(),
            defer_loading: tool.loading_policy == ToolLoadingPolicy::Deferred && !force_immediate,
        }
    }

    fn tool_search() -> Self {
        Self::ToolSearch {}
    }
}

fn build_openai_tools(
    tools: &[ToolSpec],
    tool_choice: Option<&ToolChoice>,
    tool_search_mode: ToolSearchMode,
    provider_name: &str,
) -> Result<Vec<OpenAITool>, ProviderError> {
    let forced_tool_name = match tool_choice {
        Some(ToolChoice::Tool { name }) => Some(name.as_str()),
        _ => None,
    };

    let has_deferred_tools = tools.iter().any(|tool| {
        tool.loading_policy == ToolLoadingPolicy::Deferred
            && forced_tool_name != Some(tool.name.as_str())
    });

    if has_deferred_tools && tool_search_mode != ToolSearchMode::Hosted {
        return Err(ProviderError::InvalidRequest(format!(
            "{provider_name} deferred tools require hosted tool search"
        )));
    }

    let mut provider_tools = tools
        .iter()
        .map(|tool| OpenAITool::function(tool, forced_tool_name == Some(tool.name.as_str())))
        .collect::<Vec<_>>();

    if has_deferred_tools {
        provider_tools.push(OpenAITool::tool_search());
    }

    Ok(provider_tools)
}

#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum OpenAIToolChoice {
    Mode(OpenAIToolChoiceMode),
    Function(OpenAIToolChoiceFunction),
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OpenAIToolChoiceMode {
    Auto,
    Required,
}

#[derive(Serialize)]
pub(crate) struct OpenAIToolChoiceFunction {
    #[serde(rename = "type")]
    kind: &'static str,
    name: String,
}

impl From<ToolChoice> for OpenAIToolChoice {
    fn from(choice: ToolChoice) -> Self {
        match choice {
            ToolChoice::Auto => OpenAIToolChoice::Mode(OpenAIToolChoiceMode::Auto),
            ToolChoice::Any => OpenAIToolChoice::Mode(OpenAIToolChoiceMode::Required),
            ToolChoice::Tool { name } => OpenAIToolChoice::Function(OpenAIToolChoiceFunction {
                kind: "function",
                name,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{borrow::Cow, collections::BTreeMap};

    use serde_json::json;
    use time::OffsetDateTime;

    use crate::{
        BuiltinProvider,
        provider::model::{
            ContentBlock, Message, OpenAIRequestOptions, ProviderError, ProviderRequestOptions,
            Request, Role, ToolChoice, ToolSearchMode,
        },
        tool::{ToolLoadingPolicy, ToolSpec},
    };

    use super::{OpenAIModel, OpenAIResponsesRequest};

    #[test]
    fn converts_request_to_responses_payload() {
        let request = Request {
            model: Cow::Borrowed("gpt-5"),
            system: Some(Cow::Borrowed("Be helpful.")),
            messages: Cow::Owned(vec![
                Message::user(ContentBlock::text("What files changed?")),
                Message::assistant(ContentBlock::text("I'll inspect that.")),
                Message::assistant(ContentBlock::ToolUse {
                    id: "call_123".to_string(),
                    name: "files".to_string(),
                    input: json!({ "operations": [{ "op": "read", "path": "README.md" }] }),
                }),
                Message::assistant(ContentBlock::ToolResult {
                    tool_use_id: "call_123".to_string(),
                    content: "README contents".to_string(),
                    is_error: false,
                }),
            ]),
            tools: Cow::Owned(vec![ToolSpec {
                name: "files".to_string(),
                description: Some("Read and edit files".to_string()),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "operations": { "type": "array" }
                    }
                }),
                capabilities: vec![],
                side_effect_level: crate::tool::ToolSideEffectLevel::None,
                durability: crate::tool::ToolDurability::ReplaySafe,
                loading_policy: crate::tool::ToolLoadingPolicy::Immediate,
                execution_timeout: None,
            }]),
            tool_choice: Some(ToolChoice::Tool {
                name: "files".to_string(),
            }),
            temperature: Some(0.2),
            max_output_tokens: Some(256),
            metadata: Cow::Owned(BTreeMap::from([(
                "agent".to_string(),
                "mentra".to_string(),
            )])),
            provider_request_options: ProviderRequestOptions::default(),
        };

        let payload = serde_json::to_value(OpenAIResponsesRequest::try_from(request).unwrap())
            .expect("request should serialize");

        assert_eq!(payload["model"], "gpt-5");
        assert_eq!(payload["instructions"], "Be helpful.");
        assert_eq!(payload["input"][0]["role"], "user");
        assert_eq!(payload["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(
            payload["input"][0]["content"][0]["text"],
            "What files changed?"
        );
        assert_eq!(payload["input"][1]["role"], "assistant");
        assert_eq!(payload["input"][1]["content"][0]["type"], "output_text");
        assert_eq!(
            payload["input"][1]["content"][0]["text"],
            "I'll inspect that."
        );
        assert_eq!(payload["input"][2]["type"], "function_call");
        assert_eq!(payload["input"][2]["call_id"], "call_123");
        assert_eq!(payload["input"][2]["name"], "files");
        assert_eq!(payload["input"][3]["type"], "function_call_output");
        assert_eq!(payload["input"][3]["output"], "README contents");
        assert_eq!(payload["tools"][0]["type"], "function");
        assert!(payload["tools"][0].get("defer_loading").is_none());
        assert_eq!(payload["tool_choice"]["type"], "function");
        assert_eq!(payload["tool_choice"]["name"], "files");
        let temperature = payload["temperature"]
            .as_f64()
            .expect("temperature should be numeric");
        assert!((temperature - 0.2).abs() < 1e-6);
        assert_eq!(payload["max_output_tokens"], 256);
        assert_eq!(payload["metadata"]["agent"], "mentra");
    }

    #[test]
    fn prefixes_tool_errors_in_function_call_output() {
        let request = Request {
            model: Cow::Borrowed("gpt-5"),
            system: None,
            messages: Cow::Owned(vec![Message::user(ContentBlock::ToolResult {
                tool_use_id: "call_456".to_string(),
                content: "No such file".to_string(),
                is_error: true,
            })]),
            tools: Cow::Owned(vec![]),
            tool_choice: Some(ToolChoice::Auto),
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
            provider_request_options: ProviderRequestOptions::default(),
        };

        let payload = serde_json::to_value(OpenAIResponsesRequest::try_from(request).unwrap())
            .expect("request should serialize");

        assert_eq!(payload["input"][0]["output"], "Tool error: No such file");
        assert_eq!(payload["tool_choice"], "auto");
    }

    #[test]
    fn serializes_inline_images_as_input_image_parts() {
        let request = Request {
            model: Cow::Borrowed("gpt-5"),
            system: None,
            messages: Cow::Owned(vec![Message {
                role: Role::User,
                content: vec![
                    ContentBlock::text("What is in this image?"),
                    ContentBlock::image_bytes("image/png", [1_u8, 2, 3]),
                ],
            }]),
            tools: Cow::Owned(vec![]),
            tool_choice: Some(ToolChoice::Auto),
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
            provider_request_options: ProviderRequestOptions::default(),
        };

        let payload = serde_json::to_value(OpenAIResponsesRequest::try_from(request).unwrap())
            .expect("request should serialize");

        assert_eq!(payload["input"][0]["role"], "user");
        assert_eq!(payload["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(
            payload["input"][0]["content"][0]["text"],
            "What is in this image?"
        );
        assert_eq!(payload["input"][0]["content"][1]["type"], "input_image");
        assert_eq!(
            payload["input"][0]["content"][1]["image_url"],
            "data:image/png;base64,AQID"
        );
    }

    #[test]
    fn serializes_assistant_text_as_output_text() {
        let request = Request {
            model: Cow::Borrowed("gpt-5"),
            system: None,
            messages: Cow::Owned(vec![Message::assistant(ContentBlock::text("Done."))]),
            tools: Cow::Owned(vec![]),
            tool_choice: Some(ToolChoice::Auto),
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
            provider_request_options: ProviderRequestOptions::default(),
        };

        let payload = serde_json::to_value(OpenAIResponsesRequest::try_from(request).unwrap())
            .expect("request should serialize");

        assert_eq!(payload["input"][0]["role"], "assistant");
        assert_eq!(payload["input"][0]["content"][0]["type"], "output_text");
        assert_eq!(payload["input"][0]["content"][0]["text"], "Done.");
    }

    #[test]
    fn converts_openai_unix_timestamp_to_offset_datetime() {
        let model = OpenAIModel {
            id: "gpt-5".to_string(),
            name: None,
            description: None,
            owned_by: None,
            created: Some(1_741_049_700),
        };

        let info = crate::provider::model::ModelInfo::from(model);

        assert_eq!(
            info.created_at,
            Some(OffsetDateTime::from_unix_timestamp(1_741_049_700).expect("valid timestamp"))
        );
    }

    #[test]
    fn serializes_parallel_tool_calls_option() {
        let request = Request {
            model: Cow::Borrowed("gpt-5"),
            system: None,
            messages: Cow::Owned(vec![]),
            tools: Cow::Owned(vec![]),
            tool_choice: Some(ToolChoice::Auto),
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
            provider_request_options: ProviderRequestOptions {
                tool_search_mode: crate::provider::ToolSearchMode::Disabled,
                openai: OpenAIRequestOptions {
                    parallel_tool_calls: Some(true),
                },
                anthropic: Default::default(),
            },
        };

        let payload = serde_json::to_value(OpenAIResponsesRequest::try_from(request).unwrap())
            .expect("request should serialize");

        assert_eq!(payload["parallel_tool_calls"], true);
    }

    #[test]
    fn hosted_tool_search_adds_search_tool_for_deferred_tools() {
        let request = Request {
            model: Cow::Borrowed("gpt-5.4"),
            system: None,
            messages: Cow::Owned(vec![Message::user(ContentBlock::text("hello"))]),
            tools: Cow::Owned(vec![ToolSpec {
                name: "lookup_order".to_string(),
                description: Some("Look up an order".to_string()),
                input_schema: json!({"type":"object"}),
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

        let payload = serde_json::to_value(OpenAIResponsesRequest::try_from(request).unwrap())
            .expect("request should serialize");

        assert_eq!(payload["tools"][0]["type"], "function");
        assert_eq!(payload["tools"][0]["name"], "lookup_order");
        assert_eq!(payload["tools"][0]["defer_loading"], true);
        assert_eq!(payload["tools"][1]["type"], "tool_search");
    }

    #[test]
    fn converts_openrouter_model_metadata() {
        let model = OpenAIModel {
            id: "openai/gpt-4.1-mini".to_string(),
            name: Some("GPT-4.1 Mini".to_string()),
            description: Some("Fast multimodal model".to_string()),
            owned_by: None,
            created: Some(1_741_049_700),
        };

        let info = model.into_model_info(BuiltinProvider::OpenRouter);

        assert_eq!(info.provider, BuiltinProvider::OpenRouter.into());
        assert_eq!(info.display_name.as_deref(), Some("GPT-4.1 Mini"));
        assert_eq!(info.description.as_deref(), Some("Fast multimodal model"));
    }

    #[test]
    fn rejects_deferred_tools_without_hosted_tool_search() {
        let request = Request {
            model: Cow::Borrowed("gpt-5.4"),
            system: None,
            messages: Cow::Owned(vec![]),
            tools: Cow::Owned(vec![ToolSpec {
                name: "lookup_order".to_string(),
                description: None,
                input_schema: json!({"type":"object"}),
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

        let error = OpenAIResponsesRequest::try_from(request)
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
    fn rejects_openrouter_deferred_tools_without_hosted_tool_search() {
        let request = Request {
            model: Cow::Borrowed("openai/gpt-4.1-mini"),
            system: None,
            messages: Cow::Owned(vec![]),
            tools: Cow::Owned(vec![ToolSpec {
                name: "lookup_order".to_string(),
                description: None,
                input_schema: json!({"type":"object"}),
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

        let error = OpenAIResponsesRequest::try_from_request(request, "OpenRouter")
            .err()
            .expect("request should fail");
        match error {
            ProviderError::InvalidRequest(message) => {
                assert!(message.contains("OpenRouter deferred tools require hosted tool search"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn forced_deferred_tool_serializes_as_immediate() {
        let request = Request {
            model: Cow::Borrowed("gpt-5.4"),
            system: None,
            messages: Cow::Owned(vec![]),
            tools: Cow::Owned(vec![ToolSpec {
                name: "lookup_order".to_string(),
                description: Some("Look up an order".to_string()),
                input_schema: json!({"type":"object"}),
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

        let payload = serde_json::to_value(OpenAIResponsesRequest::try_from(request).unwrap())
            .expect("request should serialize");

        assert_eq!(payload["tools"][0]["type"], "function");
        assert!(payload["tools"][0].get("defer_loading").is_none());
        assert!(payload["tools"].get(1).is_none());
        assert_eq!(payload["tool_choice"]["name"], "lookup_order");
    }
}
