use std::borrow::Cow;
use std::collections::BTreeMap;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ProviderId;
use crate::{
    ContentBlock, HostedToolSearchCall, HostedWebSearchCall, ImageGenerationCall,
    ImageGenerationResult, ImageSource, Message, ModelInfo, ProviderError, ReasoningEffort,
    ReasoningOptions, ReasoningSummary, Request, ResponsesTextControls, Role, ToolChoice,
    ToolSearchMode, WebSearchAction,
};

use crate::tool::{ProviderToolKind, ToolLoadingPolicy, ToolSpec};

/// Page returned by the Responses-compatible models endpoint.
#[derive(Debug, Deserialize)]
pub struct ResponsesModelsPage {
    data: Vec<ResponsesModel>,
}

impl ResponsesModelsPage {
    pub fn into_model_info(self, provider: ProviderId) -> Vec<ModelInfo> {
        self.data
            .into_iter()
            .map(|model| model.into_model_info(provider.clone()))
            .collect()
    }
}

#[derive(Debug, Deserialize)]
pub struct ResponsesModel {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub owned_by: Option<String>,
    #[serde(default)]
    pub created: Option<u64>,
}

impl ResponsesModel {
    pub fn into_model_info(self, provider: ProviderId) -> ModelInfo {
        ModelInfo {
            id: self.id,
            provider,
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

#[derive(Debug, Serialize)]
pub struct ResponsesRequest {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    input: Vec<ResponsesInputItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ResponsesTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ResponsesToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    store: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    include: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<ResponsesTextControls>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ResponsesReasoning>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    metadata: BTreeMap<String, String>,
}

impl<'a> TryFrom<Request<'a>> for ResponsesRequest {
    type Error = ProviderError;

    fn try_from(value: Request<'a>) -> Result<Self, Self::Error> {
        Self::try_from_request(value, "Responses")
    }
}

impl ResponsesRequest {
    pub fn try_from_request<'a>(
        value: Request<'a>,
        provider_name: &str,
    ) -> Result<Self, ProviderError> {
        let mut input = Vec::new();

        for message in value.messages.iter() {
            input.extend(ResponsesInputItem::from_message(message)?);
        }

        Ok(Self {
            model: value.model.into_owned(),
            instructions: value.system.map(Cow::into_owned),
            input,
            tools: build_responses_tools(
                value.tools.as_ref(),
                value.tool_choice.as_ref(),
                value.provider_request_options.tool_search_mode,
                provider_name,
            )?,
            tool_choice: value.tool_choice.map(Into::into),
            temperature: value.temperature,
            max_output_tokens: value.max_output_tokens,
            parallel_tool_calls: value.provider_request_options.responses.parallel_tool_calls,
            store: value.provider_request_options.responses.store,
            stream: value.provider_request_options.responses.stream,
            include: value.provider_request_options.responses.include.clone(),
            service_tier: value
                .provider_request_options
                .responses
                .service_tier
                .clone(),
            prompt_cache_key: value
                .provider_request_options
                .responses
                .prompt_cache_key
                .clone(),
            text: value.provider_request_options.responses.text.clone(),
            reasoning: value
                .provider_request_options
                .reasoning
                .map(ResponsesReasoning::from),
            metadata: value.metadata.into_owned(),
        })
    }
}

#[derive(Debug, Serialize)]
struct ResponsesReasoning {
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<ResponsesReasoningEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<ResponsesReasoningSummary>,
}

impl From<ReasoningOptions> for ResponsesReasoning {
    fn from(value: ReasoningOptions) -> Self {
        Self {
            effort: value.effort.map(Into::into),
            summary: value.summary.map(Into::into),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ResponsesReasoningEffort {
    Low,
    Medium,
    High,
}

impl From<ReasoningEffort> for ResponsesReasoningEffort {
    fn from(value: ReasoningEffort) -> Self {
        match value {
            ReasoningEffort::Low => Self::Low,
            ReasoningEffort::Medium => Self::Medium,
            ReasoningEffort::High => Self::High,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum ResponsesReasoningSummary {
    Auto,
    Concise,
    Detailed,
}

impl From<ReasoningSummary> for ResponsesReasoningSummary {
    fn from(value: ReasoningSummary) -> Self {
        match value {
            ReasoningSummary::Auto => Self::Auto,
            ReasoningSummary::Concise => Self::Concise,
            ReasoningSummary::Detailed => Self::Detailed,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ResponsesInputItem {
    Message(ResponsesMessageInput),
    FunctionCall(ResponsesFunctionCallInput),
    FunctionCallOutput(ResponsesFunctionCallOutputInput),
    ToolSearchCall(ResponsesToolSearchCallInput),
    WebSearchCall(ResponsesWebSearchCallInput),
    ImageGenerationCall(ResponsesImageGenerationCallInput),
}

impl ResponsesInputItem {
    fn from_message(message: &Message) -> Result<Vec<Self>, ProviderError> {
        let mut items = Vec::new();
        let mut content = Vec::new();
        let mut text_buffer = String::new();

        for block in &message.content {
            match block {
                ContentBlock::Text { text } => text_buffer.push_str(text),
                ContentBlock::Image { source } => {
                    Self::flush_text(&mut text_buffer, &message.role, &mut content)?;
                    content.push(ResponsesMessageContentPart::try_from((
                        source,
                        &message.role,
                    ))?);
                }
                ContentBlock::ToolUse { id, name, input } => {
                    Self::flush_message(message, &mut text_buffer, &mut content, &mut items)?;
                    items.push(Self::FunctionCall(ResponsesFunctionCallInput {
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
                    items.push(Self::FunctionCallOutput(ResponsesFunctionCallOutputInput {
                        kind: "function_call_output",
                        call_id: tool_use_id.clone(),
                        output: render_tool_output(&tool_output.to_display_string(), *is_error),
                    }));
                }
                ContentBlock::HostedToolSearch { call } => {
                    Self::flush_message(message, &mut text_buffer, &mut content, &mut items)?;
                    items.push(Self::ToolSearchCall(ResponsesToolSearchCallInput::from(
                        call.clone(),
                    )));
                }
                ContentBlock::HostedWebSearch { call } => {
                    Self::flush_message(message, &mut text_buffer, &mut content, &mut items)?;
                    items.push(Self::WebSearchCall(ResponsesWebSearchCallInput::from(
                        call.clone(),
                    )));
                }
                ContentBlock::ImageGeneration { call } => {
                    Self::flush_message(message, &mut text_buffer, &mut content, &mut items)?;
                    items.push(Self::ImageGenerationCall(
                        ResponsesImageGenerationCallInput::from(call.clone()),
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
        content: &mut Vec<ResponsesMessageContentPart>,
    ) -> Result<(), ProviderError> {
        if text_buffer.is_empty() {
            return Ok(());
        }

        content.push(ResponsesMessageContentPart::text_for_role(
            role,
            std::mem::take(text_buffer),
        )?);
        Ok(())
    }

    fn flush_message(
        message: &Message,
        text_buffer: &mut String,
        content: &mut Vec<ResponsesMessageContentPart>,
        items: &mut Vec<Self>,
    ) -> Result<(), ProviderError> {
        Self::flush_text(text_buffer, &message.role, content)?;
        if content.is_empty() {
            return Ok(());
        }

        items.push(Self::Message(ResponsesMessageInput {
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

#[derive(Debug, Serialize)]
pub struct ResponsesMessageInput {
    role: String,
    content: Vec<ResponsesMessageContentPart>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesMessageContentPart {
    InputText { text: String },
    OutputText { text: String },
    InputImage { image_url: String },
}

impl ResponsesMessageContentPart {
    fn text_for_role(role: &Role, text: String) -> Result<Self, ProviderError> {
        match role {
            Role::User => Ok(Self::InputText { text }),
            Role::Assistant => Ok(Self::OutputText { text }),
            Role::Unknown(role) => Err(ProviderError::InvalidRequest(format!(
                "Responses message role '{role}' is not supported for text content"
            ))),
        }
    }
}

impl TryFrom<(&ImageSource, &Role)> for ResponsesMessageContentPart {
    type Error = ProviderError;

    fn try_from(value: (&ImageSource, &Role)) -> Result<Self, Self::Error> {
        let (source, role) = value;
        if !matches!(role, Role::User) {
            return Err(ProviderError::InvalidRequest(
                "Responses image inputs are only supported in user messages".to_string(),
            ));
        }

        let image_url = match source {
            ImageSource::Bytes { media_type, data } => {
                format!("data:{media_type};base64,{}", STANDARD.encode(data))
            }
            ImageSource::Url { url } => url.clone(),
        };

        Ok(ResponsesMessageContentPart::InputImage { image_url })
    }
}

#[derive(Debug, Serialize)]
pub struct ResponsesFunctionCallInput {
    #[serde(rename = "type")]
    kind: &'static str,
    call_id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Serialize)]
pub struct ResponsesFunctionCallOutputInput {
    #[serde(rename = "type")]
    kind: &'static str,
    call_id: String,
    output: String,
}

#[derive(Debug, Serialize)]
pub struct ResponsesToolSearchCallInput {
    #[serde(rename = "type")]
    kind: &'static str,
    call_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    execution: &'static str,
    arguments: serde_json::Value,
}

impl From<HostedToolSearchCall> for ResponsesToolSearchCallInput {
    fn from(value: HostedToolSearchCall) -> Self {
        Self {
            kind: "tool_search_call",
            call_id: value.id,
            status: value.status,
            execution: "client",
            arguments: serde_json::json!({
                "query": value.query,
            }),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ResponsesWebSearchCallInput {
    #[serde(rename = "type")]
    kind: &'static str,
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<WebSearchAction>,
}

impl From<HostedWebSearchCall> for ResponsesWebSearchCallInput {
    fn from(value: HostedWebSearchCall) -> Self {
        Self {
            kind: "web_search_call",
            id: value.id,
            status: value.status,
            action: value.action,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ResponsesImageGenerationCallInput {
    #[serde(rename = "type")]
    kind: &'static str,
    id: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    revised_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<String>,
}

impl From<ImageGenerationCall> for ResponsesImageGenerationCallInput {
    fn from(value: ImageGenerationCall) -> Self {
        Self {
            kind: "image_generation_call",
            id: value.id,
            status: value.status,
            revised_prompt: value.revised_prompt,
            result: value.result.map(render_image_generation_result),
        }
    }
}

fn render_image_generation_result(result: ImageGenerationResult) -> String {
    match result {
        ImageGenerationResult::ArtifactRef { artifact_id } => artifact_id,
        ImageGenerationResult::Image { source } => match source {
            ImageSource::Bytes { data, .. } => STANDARD.encode(data),
            ImageSource::Url { url } => url,
        },
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesTool {
    Function {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        parameters: serde_json::Value,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        defer_loading: bool,
    },
    ToolSearch {},
    WebSearch {
        #[serde(skip_serializing_if = "Option::is_none")]
        external_web_access: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        filters: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        user_location: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        search_context_size: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        search_content_types: Option<Vec<String>>,
    },
    ImageGeneration {
        output_format: String,
    },
}

#[derive(Debug, Deserialize)]
struct ResponsesWebSearchToolOptions {
    #[serde(default)]
    external_web_access: Option<bool>,
    #[serde(default)]
    filters: Option<serde_json::Value>,
    #[serde(default)]
    user_location: Option<serde_json::Value>,
    #[serde(default)]
    search_context_size: Option<serde_json::Value>,
    #[serde(default)]
    search_content_types: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct ResponsesImageGenerationToolOptions {
    output_format: String,
}

impl ResponsesTool {
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

    fn web_search(tool: &ToolSpec) -> Result<Self, ProviderError> {
        let options = tool.options.clone().ok_or_else(|| {
            ProviderError::InvalidRequest("web_search tools require provider options".to_string())
        })?;
        let options: ResponsesWebSearchToolOptions =
            serde_json::from_value(options).map_err(|error| {
                ProviderError::InvalidRequest(format!("invalid web_search tool options: {error}"))
            })?;
        Ok(Self::WebSearch {
            external_web_access: options.external_web_access,
            filters: options.filters,
            user_location: options.user_location,
            search_context_size: options.search_context_size,
            search_content_types: options.search_content_types,
        })
    }

    fn image_generation(tool: &ToolSpec) -> Result<Self, ProviderError> {
        let options = tool.options.clone().ok_or_else(|| {
            ProviderError::InvalidRequest(
                "image_generation tools require provider options".to_string(),
            )
        })?;
        let options: ResponsesImageGenerationToolOptions = serde_json::from_value(options)
            .map_err(|error| {
                ProviderError::InvalidRequest(format!(
                    "invalid image_generation tool options: {error}"
                ))
            })?;
        Ok(Self::ImageGeneration {
            output_format: options.output_format,
        })
    }
}

fn build_responses_tools(
    tools: &[ToolSpec],
    tool_choice: Option<&ToolChoice>,
    tool_search_mode: ToolSearchMode,
    provider_name: &str,
) -> Result<Vec<ResponsesTool>, ProviderError> {
    let forced_tool_name = match tool_choice {
        Some(ToolChoice::Tool { name }) => Some(name.as_str()),
        _ => None,
    };

    let has_deferred_tools = tools.iter().any(|tool| {
        tool.kind == ProviderToolKind::Function
            && tool.loading_policy == ToolLoadingPolicy::Deferred
            && forced_tool_name != Some(tool.name.as_str())
    });

    if has_deferred_tools && tool_search_mode != ToolSearchMode::Hosted {
        return Err(ProviderError::InvalidRequest(format!(
            "{provider_name} deferred tools require hosted tool search"
        )));
    }

    let mut provider_tools = Vec::with_capacity(tools.len() + usize::from(has_deferred_tools));
    for tool in tools {
        let provider_tool = match tool.kind {
            ProviderToolKind::Function => {
                ResponsesTool::function(tool, forced_tool_name == Some(tool.name.as_str()))
            }
            ProviderToolKind::HostedWebSearch => ResponsesTool::web_search(tool)?,
            ProviderToolKind::ImageGeneration => ResponsesTool::image_generation(tool)?,
        };
        provider_tools.push(provider_tool);
    }

    if has_deferred_tools {
        provider_tools.push(ResponsesTool::tool_search());
    }

    Ok(provider_tools)
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ResponsesToolChoice {
    Mode(ResponsesToolChoiceMode),
    Function(ResponsesToolChoiceFunction),
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsesToolChoiceMode {
    Auto,
    Required,
}

#[derive(Debug, Serialize)]
pub struct ResponsesToolChoiceFunction {
    #[serde(rename = "type")]
    kind: &'static str,
    name: String,
}

impl From<ToolChoice> for ResponsesToolChoice {
    fn from(choice: ToolChoice) -> Self {
        match choice {
            ToolChoice::Auto => ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Auto),
            ToolChoice::Any => ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Required),
            ToolChoice::Tool { name } => {
                ResponsesToolChoice::Function(ResponsesToolChoiceFunction {
                    kind: "function",
                    name,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{borrow::Cow, collections::BTreeMap};

    use serde_json::json;
    use time::OffsetDateTime;

    use crate::{
        ContentBlock, HostedToolSearchCall, HostedWebSearchCall, ImageGenerationCall,
        ImageGenerationResult, Message, ProviderError, ProviderRequestOptions, ReasoningEffort,
        ReasoningOptions, ReasoningSummary, Request, ResponsesRequestCompression,
        ResponsesRequestOptions, ResponsesTextControls, ResponsesTextFormat, ResponsesVerbosity,
        Role, ToolChoice, ToolLoadingPolicy, ToolResultContent, ToolSearchMode, ToolSpec,
        WebSearchAction,
    };

    use super::{ResponsesModel, ResponsesModelsPage, ResponsesRequest};

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
                    content: ToolResultContent::text("README contents"),
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
                output_schema: None,
                kind: crate::ProviderToolKind::Function,
                loading_policy: crate::tool::ToolLoadingPolicy::Immediate,
                options: None,
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

        let payload = serde_json::to_value(ResponsesRequest::try_from(request).unwrap())
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
                content: ToolResultContent::text("No such file"),
                is_error: true,
            })]),
            tools: Cow::Owned(vec![]),
            tool_choice: Some(ToolChoice::Auto),
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
            provider_request_options: ProviderRequestOptions::default(),
        };

        let payload = serde_json::to_value(ResponsesRequest::try_from(request).unwrap())
            .expect("request should serialize");

        assert_eq!(payload["input"][0]["output"], "Tool error: No such file");
        assert_eq!(payload["tool_choice"], "auto");
    }

    #[test]
    fn preserves_hosted_actions_in_responses_history_replay() {
        let request = Request {
            model: Cow::Borrowed("gpt-5"),
            system: None,
            messages: Cow::Owned(vec![Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::HostedToolSearch {
                        call: HostedToolSearchCall {
                            id: "search_1".to_string(),
                            status: Some("completed".to_string()),
                            query: Some("weather".to_string()),
                        },
                    },
                    ContentBlock::HostedWebSearch {
                        call: HostedWebSearchCall {
                            id: "web_1".to_string(),
                            status: Some("completed".to_string()),
                            action: Some(WebSearchAction::Search {
                                query: Some("weather".to_string()),
                                queries: None,
                            }),
                        },
                    },
                    ContentBlock::ImageGeneration {
                        call: ImageGenerationCall {
                            id: "image_1".to_string(),
                            status: "completed".to_string(),
                            revised_prompt: Some("A blue square".to_string()),
                            result: Some(ImageGenerationResult::ArtifactRef {
                                artifact_id: "artifact_1".to_string(),
                            }),
                        },
                    },
                ],
            }]),
            tools: Cow::Owned(vec![]),
            tool_choice: Some(ToolChoice::Auto),
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
            provider_request_options: ProviderRequestOptions::default(),
        };

        let payload = serde_json::to_value(ResponsesRequest::try_from(request).unwrap())
            .expect("request should serialize");

        assert_eq!(payload["input"][0]["type"], "tool_search_call");
        assert_eq!(payload["input"][0]["call_id"], "search_1");
        assert_eq!(payload["input"][1]["type"], "web_search_call");
        assert_eq!(payload["input"][1]["id"], "web_1");
        assert_eq!(payload["input"][2]["type"], "image_generation_call");
        assert_eq!(payload["input"][2]["id"], "image_1");
        assert_eq!(payload["input"][2]["result"], "artifact_1");
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

        let payload = serde_json::to_value(ResponsesRequest::try_from(request).unwrap())
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

        let payload = serde_json::to_value(ResponsesRequest::try_from(request).unwrap())
            .expect("request should serialize");

        assert_eq!(payload["input"][0]["role"], "assistant");
        assert_eq!(payload["input"][0]["content"][0]["type"], "output_text");
        assert_eq!(payload["input"][0]["content"][0]["text"], "Done.");
    }

    #[test]
    fn converts_unix_timestamp_to_offset_datetime() {
        let model = ResponsesModel {
            id: "gpt-5".to_string(),
            name: None,
            description: None,
            owned_by: None,
            created: Some(1_741_049_700),
        };

        let info = model.into_model_info(crate::ProviderId::new("openai"));

        assert_eq!(
            info.created_at,
            Some(OffsetDateTime::from_unix_timestamp(1_741_049_700).expect("valid timestamp"))
        );
    }

    #[test]
    fn converts_model_list_page_into_model_info() {
        let page: ResponsesModelsPage = serde_json::from_str(
            r#"{
                "data": [
                    {
                        "id": "gpt-5",
                        "name": "GPT-5",
                        "description": "General-purpose model",
                        "created": 1741049700
                    },
                    {
                        "id": "gpt-5-mini",
                        "owned_by": "openai"
                    }
                ]
            }"#,
        )
        .expect("page should parse");

        let models = page.into_model_info(crate::ProviderId::new("openai"));

        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "gpt-5");
        assert_eq!(models[0].display_name.as_deref(), Some("GPT-5"));
        assert_eq!(
            models[0].description.as_deref(),
            Some("General-purpose model")
        );
        assert_eq!(models[1].description.as_deref(), Some("Owned by openai"));
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
                tool_search_mode: ToolSearchMode::Disabled,
                reasoning: None,
                responses: ResponsesRequestOptions {
                    parallel_tool_calls: Some(true),
                    ..Default::default()
                },
                anthropic: Default::default(),
                gemini: Default::default(),
                session: Default::default(),
            },
        };

        let payload = serde_json::to_value(ResponsesRequest::try_from(request).unwrap())
            .expect("request should serialize");

        assert_eq!(payload["parallel_tool_calls"], true);
    }

    #[test]
    fn serializes_reasoning_effort_option() {
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
                reasoning: Some(ReasoningOptions {
                    effort: Some(ReasoningEffort::High),
                    summary: None,
                }),
                ..Default::default()
            },
        };

        let payload = serde_json::to_value(ResponsesRequest::try_from(request).unwrap())
            .expect("request should serialize");

        assert_eq!(payload["reasoning"]["effort"], "high");
    }

    #[test]
    fn serializes_advanced_responses_request_controls() {
        let request = Request {
            model: Cow::Borrowed("gpt-5"),
            system: Some(Cow::Borrowed("Be structured.")),
            messages: Cow::Owned(vec![]),
            tools: Cow::Owned(vec![]),
            tool_choice: Some(ToolChoice::Auto),
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
            provider_request_options: ProviderRequestOptions {
                reasoning: Some(ReasoningOptions {
                    effort: None,
                    summary: Some(ReasoningSummary::Detailed),
                }),
                responses: ResponsesRequestOptions {
                    parallel_tool_calls: Some(true),
                    store: Some(false),
                    stream: Some(true),
                    include: vec!["reasoning.encrypted_content".to_string()],
                    service_tier: Some("priority".to_string()),
                    prompt_cache_key: Some("thread-123".to_string()),
                    text: Some(ResponsesTextControls {
                        verbosity: Some(ResponsesVerbosity::High),
                        format: Some(ResponsesTextFormat {
                            r#type: crate::ResponsesTextFormatType::JsonSchema,
                            strict: true,
                            schema: json!({"type":"object"}),
                            name: "codex_output_schema".to_string(),
                        }),
                    }),
                    compression: ResponsesRequestCompression::None,
                },
                ..Default::default()
            },
        };

        let payload = serde_json::to_value(ResponsesRequest::try_from(request).unwrap())
            .expect("request should serialize");

        assert_eq!(payload["store"], false);
        assert_eq!(payload["stream"], true);
        assert_eq!(payload["include"][0], "reasoning.encrypted_content");
        assert_eq!(payload["service_tier"], "priority");
        assert_eq!(payload["prompt_cache_key"], "thread-123");
        assert_eq!(payload["reasoning"]["summary"], "detailed");
        assert_eq!(payload["text"]["verbosity"], "high");
        assert_eq!(payload["text"]["format"]["name"], "codex_output_schema");
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
                output_schema: None,
                kind: crate::ProviderToolKind::Function,
                loading_policy: ToolLoadingPolicy::Deferred,
                options: None,
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

        let payload = serde_json::to_value(ResponsesRequest::try_from(request).unwrap())
            .expect("request should serialize");

        assert_eq!(payload["tools"][0]["type"], "function");
        assert_eq!(payload["tools"][0]["name"], "lookup_order");
        assert_eq!(payload["tools"][0]["defer_loading"], true);
        assert_eq!(payload["tools"][1]["type"], "tool_search");
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
                output_schema: None,
                kind: crate::ProviderToolKind::Function,
                loading_policy: ToolLoadingPolicy::Deferred,
                options: None,
            }]),
            tool_choice: Some(ToolChoice::Auto),
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
            provider_request_options: ProviderRequestOptions::default(),
        };

        let error = ResponsesRequest::try_from(request).expect_err("request should fail");
        match error {
            ProviderError::InvalidRequest(message) => {
                assert!(message.contains("deferred tools require hosted tool search"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
