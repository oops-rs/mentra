use std::{borrow::Cow, collections::BTreeMap};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{
    provider::model::{ContentBlock, Message, ModelInfo, ModelProviderKind, Request, ToolChoice},
    tool::ToolSpec,
};

#[derive(Deserialize)]
pub(crate) struct OpenAIModelsPage {
    pub(crate) data: Vec<OpenAIModel>,
}

#[derive(Deserialize)]
pub(crate) struct OpenAIModel {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) owned_by: Option<String>,
    #[serde(default)]
    pub(crate) created: Option<u64>,
}

impl From<OpenAIModel> for ModelInfo {
    fn from(model: OpenAIModel) -> Self {
        ModelInfo {
            id: model.id,
            provider: ModelProviderKind::OpenAI,
            display_name: None,
            description: model.owned_by.map(|owner| format!("Owned by {owner}")),
            created_at: model
                .created
                .and_then(|timestamp| OffsetDateTime::from_unix_timestamp(timestamp as i64).ok()),
        }
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
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    metadata: BTreeMap<String, String>,
}

impl<'a> From<Request<'a>> for OpenAIResponsesRequest {
    fn from(value: Request<'a>) -> Self {
        let mut input = Vec::new();

        for message in value.messages.iter() {
            input.extend(OpenAIInputItem::from_message(message));
        }

        OpenAIResponsesRequest {
            model: value.model.into_owned(),
            instructions: value.system.map(Cow::into_owned),
            input,
            tools: value.tools.iter().map(|tool| tool.into()).collect(),
            tool_choice: value.tool_choice.map(Into::into),
            temperature: value.temperature,
            max_output_tokens: value.max_output_tokens,
            metadata: value.metadata.into_owned(),
        }
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
    fn from_message(message: &Message) -> Vec<Self> {
        let mut items = Vec::new();
        let mut text_buffer = String::new();

        for block in &message.content {
            match block {
                ContentBlock::Text { text } => text_buffer.push_str(text),
                ContentBlock::ToolUse { id, name, input } => {
                    Self::flush_text(message, &mut text_buffer, &mut items);
                    items.push(OpenAIInputItem::FunctionCall(OpenAIFunctionCallInput {
                        kind: "function_call",
                        call_id: id.clone(),
                        name: name.clone(),
                        arguments: input.to_string(),
                    }));
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    Self::flush_text(message, &mut text_buffer, &mut items);
                    items.push(OpenAIInputItem::FunctionCallOutput(
                        OpenAIFunctionCallOutputInput {
                            kind: "function_call_output",
                            call_id: tool_use_id.clone(),
                            output: render_tool_output(content, *is_error),
                        },
                    ));
                }
            }
        }

        Self::flush_text(message, &mut text_buffer, &mut items);
        items
    }

    fn flush_text(message: &Message, text_buffer: &mut String, items: &mut Vec<Self>) {
        if text_buffer.is_empty() {
            return;
        }

        items.push(OpenAIInputItem::Message(OpenAIMessageInput {
            role: match &message.role {
                crate::provider::model::Role::User => "user".to_string(),
                crate::provider::model::Role::Assistant => "assistant".to_string(),
                crate::provider::model::Role::Unknown(role) => role.clone(),
            },
            content: std::mem::take(text_buffer),
        }));
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
    content: String,
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
pub(crate) struct OpenAITool {
    #[serde(rename = "type")]
    kind: &'static str,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    parameters: serde_json::Value,
}

impl From<ToolSpec> for OpenAITool {
    fn from(tool: ToolSpec) -> Self {
        OpenAITool::from(&tool)
    }
}

impl From<&ToolSpec> for OpenAITool {
    fn from(tool: &ToolSpec) -> Self {
        OpenAITool {
            kind: "function",
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.input_schema.clone(),
        }
    }
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
        provider::model::{ContentBlock, Message, Request, Role, ToolChoice},
        tool::ToolSpec,
    };

    use super::{OpenAIModel, OpenAIResponsesRequest};

    #[test]
    fn converts_request_to_responses_payload() {
        let request = Request {
            model: Cow::Borrowed("gpt-5"),
            system: Some(Cow::Borrowed("Be helpful.")),
            messages: Cow::Owned(vec![
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: "What files changed?".to_string(),
                    }],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "call_123".to_string(),
                        name: "read_file".to_string(),
                        input: json!({ "path": "README.md" }),
                    }],
                },
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "call_123".to_string(),
                        content: "README contents".to_string(),
                        is_error: false,
                    }],
                },
            ]),
            tools: Cow::Owned(vec![ToolSpec {
                name: "read_file".to_string(),
                description: Some("Read a file".to_string()),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    }
                }),
            }]),
            tool_choice: Some(ToolChoice::Tool {
                name: "read_file".to_string(),
            }),
            temperature: Some(0.2),
            max_output_tokens: Some(256),
            metadata: Cow::Owned(BTreeMap::from([(
                "agent".to_string(),
                "mentra".to_string(),
            )])),
        };

        let payload = serde_json::to_value(OpenAIResponsesRequest::from(request))
            .expect("request should serialize");

        assert_eq!(payload["model"], "gpt-5");
        assert_eq!(payload["instructions"], "Be helpful.");
        assert_eq!(payload["input"][0]["role"], "user");
        assert_eq!(payload["input"][0]["content"], "What files changed?");
        assert_eq!(payload["input"][1]["type"], "function_call");
        assert_eq!(payload["input"][1]["call_id"], "call_123");
        assert_eq!(payload["input"][1]["name"], "read_file");
        assert_eq!(payload["input"][2]["type"], "function_call_output");
        assert_eq!(payload["input"][2]["output"], "README contents");
        assert_eq!(payload["tools"][0]["type"], "function");
        assert_eq!(payload["tool_choice"]["type"], "function");
        assert_eq!(payload["tool_choice"]["name"], "read_file");
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
            messages: Cow::Owned(vec![Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_456".to_string(),
                    content: "No such file".to_string(),
                    is_error: true,
                }],
            }]),
            tools: Cow::Owned(vec![]),
            tool_choice: Some(ToolChoice::Auto),
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
        };

        let payload = serde_json::to_value(OpenAIResponsesRequest::from(request))
            .expect("request should serialize");

        assert_eq!(payload["input"][0]["output"], "Tool error: No such file");
        assert_eq!(payload["tool_choice"], "auto");
    }

    #[test]
    fn converts_openai_unix_timestamp_to_offset_datetime() {
        let model = OpenAIModel {
            id: "gpt-5".to_string(),
            owned_by: None,
            created: Some(1_741_049_700),
        };

        let info = crate::provider::model::ModelInfo::from(model);

        assert_eq!(
            info.created_at,
            Some(OffsetDateTime::from_unix_timestamp(1_741_049_700).expect("valid timestamp"))
        );
    }
}
