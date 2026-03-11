use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    provider::model::{
        ContentBlock, Message, ModelInfo, ModelProviderKind, Request, Response, Role, ToolChoice,
    },
    tool::ToolSpec,
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
            provider: ModelProviderKind::Anthropic,
            display_name: model.display_name,
            description: None,
            created_at: model
                .created_at
                .as_deref()
                .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok()),
        }
    }
}

#[cfg(test)]
mod tests {
    use time::{OffsetDateTime, format_description::well_known::Rfc3339};

    use super::AnthropicModel;

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
}

#[derive(Deserialize)]
pub(crate) struct AnthropicResponse {
    pub(crate) id: String,
    pub(crate) model: String,
    pub(crate) role: String,
    content: Vec<AnthropicContentBlock>,
    stop_reason: Option<String>,
}

impl From<AnthropicResponse> for Response {
    fn from(response: AnthropicResponse) -> Self {
        Response {
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
                .map(|block| block.into())
                .collect(),
            stop_reason: response.stop_reason,
        }
    }
}

impl<'a> From<Request<'a>> for AnthropicRequest {
    fn from(value: Request<'a>) -> Self {
        AnthropicRequest {
            model: value.model.into_owned(),
            system: value.system.map(Cow::into_owned),
            messages: value
                .messages
                .iter()
                .map(|message| message.into())
                .collect(),
            tools: value.tools.iter().map(|tool| tool.into()).collect(),
            tool_choice: value.tool_choice.map(|choice| choice.into()),
            temperature: value.temperature,
            max_output_tokens: value.max_output_tokens,
        }
    }
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: String,
    content: Vec<AnthropicContentBlock>,
}

impl From<Message> for AnthropicMessage {
    fn from(message: Message) -> Self {
        AnthropicMessage::from(&message)
    }
}

impl From<&Message> for AnthropicMessage {
    fn from(message: &Message) -> Self {
        AnthropicMessage {
            role: match &message.role {
                Role::User => "user".to_string(),
                Role::Assistant => "assistant".to_string(),
                Role::Unknown(role) => role.clone(),
            },
            content: message.content.iter().map(|block| block.into()).collect(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlock {
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

impl From<ContentBlock> for AnthropicContentBlock {
    fn from(block: ContentBlock) -> Self {
        AnthropicContentBlock::from(&block)
    }
}

impl From<&ContentBlock> for AnthropicContentBlock {
    fn from(block: &ContentBlock) -> Self {
        match block {
            ContentBlock::Text { text } => AnthropicContentBlock::Text { text: text.clone() },
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

impl From<AnthropicContentBlock> for ContentBlock {
    fn from(block: AnthropicContentBlock) -> Self {
        match block {
            AnthropicContentBlock::Text { text } => ContentBlock::Text { text },
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
        }
    }
}

#[derive(Serialize)]
struct AnthropicTool {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    input_schema: Value,
}

impl From<ToolSpec> for AnthropicTool {
    fn from(tool: ToolSpec) -> Self {
        AnthropicTool::from(&tool)
    }
}

impl From<&ToolSpec> for AnthropicTool {
    fn from(tool: &ToolSpec) -> Self {
        AnthropicTool {
            name: tool.name.clone(),
            description: tool.description.clone(),
            input_schema: tool.input_schema.clone(),
        }
    }
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
