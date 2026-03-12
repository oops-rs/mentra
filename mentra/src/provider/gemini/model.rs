use std::collections::BTreeMap;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    provider::model::{
        ContentBlock, ImageSource, Message, ModelInfo, ProviderError, ProviderId, Request, Role,
        ToolChoice,
    },
    tool::ToolSpec,
};

#[derive(Deserialize)]
pub(crate) struct GeminiModelsPage {
    #[serde(default)]
    pub(crate) models: Vec<GeminiModel>,
    #[serde(default, rename = "nextPageToken", alias = "next_page_token")]
    pub(crate) next_page_token: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct GeminiModel {
    pub(crate) name: String,
    #[serde(default, rename = "baseModelId", alias = "base_model_id")]
    pub(crate) base_model_id: Option<String>,
    #[serde(default, rename = "displayName", alias = "display_name")]
    pub(crate) display_name: Option<String>,
    #[serde(default)]
    pub(crate) description: Option<String>,
    #[serde(
        default,
        rename = "supportedGenerationMethods",
        alias = "supported_generation_methods"
    )]
    supported_generation_methods: Vec<String>,
}

impl GeminiModel {
    pub(crate) fn supports_generate_content(&self) -> bool {
        self.supported_generation_methods
            .iter()
            .any(|method| matches!(method.as_str(), "generateContent" | "streamGenerateContent"))
    }
}

impl From<GeminiModel> for ModelInfo {
    fn from(model: GeminiModel) -> Self {
        let id = model.base_model_id.unwrap_or_else(|| {
            model
                .name
                .strip_prefix("models/")
                .unwrap_or(&model.name)
                .to_string()
        });

        ModelInfo {
            id,
            provider: ProviderId::GEMINI,
            display_name: model.display_name,
            description: model.description,
            created_at: None,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct GeminiGenerateContentRequest {
    #[serde(rename = "systemInstruction", skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiInstruction>,
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<GeminiTool>,
    #[serde(rename = "toolConfig", skip_serializing_if = "Option::is_none")]
    tool_config: Option<GeminiToolConfig>,
    #[serde(rename = "generationConfig", skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig>,
}

impl<'a> TryFrom<Request<'a>> for GeminiGenerateContentRequest {
    type Error = ProviderError;

    fn try_from(value: Request<'a>) -> Result<Self, Self::Error> {
        let generation_config = GeminiGenerationConfig::from_request(&value);
        let tool_name_by_id = collect_tool_name_by_id(value.messages.as_ref());
        let contents = value
            .messages
            .iter()
            .map(|message| GeminiContent::try_from_message(message, &tool_name_by_id))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|content| !content.parts.is_empty())
            .collect::<Vec<_>>();
        let tools = if value.tools.is_empty() {
            Vec::new()
        } else {
            vec![GeminiTool {
                function_declarations: value
                    .tools
                    .iter()
                    .map(GeminiFunctionDeclaration::from)
                    .collect(),
            }]
        };

        Ok(GeminiGenerateContentRequest {
            system_instruction: value.system.map(|system| GeminiInstruction {
                parts: vec![GeminiPart::Text {
                    text: system.into_owned(),
                }],
            }),
            contents,
            tool_config: value
                .tool_choice
                .filter(|_| !tools.is_empty())
                .map(Into::into),
            tools,
            generation_config,
        })
    }
}

fn collect_tool_name_by_id(messages: &[Message]) -> BTreeMap<String, String> {
    let mut names = BTreeMap::new();

    for message in messages {
        for block in &message.content {
            if let ContentBlock::ToolUse { id, name, .. } = block {
                names.insert(id.clone(), name.clone());
            }
        }
    }

    names
}

#[derive(Serialize)]
struct GeminiInstruction {
    parts: Vec<GeminiPart>,
}

#[derive(Serialize)]
struct GeminiContent {
    role: String,
    parts: Vec<GeminiPart>,
}

impl GeminiContent {
    fn try_from_message(
        message: &Message,
        tool_name_by_id: &BTreeMap<String, String>,
    ) -> Result<Self, ProviderError> {
        let role = match &message.role {
            Role::User => "user".to_string(),
            Role::Assistant => "model".to_string(),
            Role::Unknown(role) => {
                return Err(ProviderError::InvalidRequest(format!(
                    "Gemini message role '{role}' is not supported"
                )));
            }
        };

        let mut parts = Vec::with_capacity(message.content.len());
        for block in &message.content {
            parts.push(GeminiPart::try_from_block(
                block,
                &message.role,
                tool_name_by_id,
            )?);
        }

        Ok(GeminiContent { role, parts })
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum GeminiPart {
    Text {
        text: String,
    },
    InlineData {
        #[serde(rename = "inlineData")]
        inline_data: GeminiInlineData,
    },
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: GeminiFunctionCall,
    },
    FunctionResponse {
        #[serde(rename = "functionResponse")]
        function_response: GeminiFunctionResponse,
    },
}

impl GeminiPart {
    fn try_from_block(
        block: &ContentBlock,
        role: &Role,
        tool_name_by_id: &BTreeMap<String, String>,
    ) -> Result<Self, ProviderError> {
        match block {
            ContentBlock::Text { text } => Ok(GeminiPart::Text { text: text.clone() }),
            ContentBlock::Image { source } => {
                if !matches!(role, Role::User) {
                    return Err(ProviderError::InvalidRequest(
                        "Gemini image inputs are only supported in user messages".to_string(),
                    ));
                }

                match source {
                    ImageSource::Bytes { media_type, data } => Ok(GeminiPart::InlineData {
                        inline_data: GeminiInlineData {
                            mime_type: media_type.clone(),
                            data: STANDARD.encode(data),
                        },
                    }),
                    ImageSource::Url { .. } => Err(ProviderError::InvalidRequest(
                        "Gemini image URL inputs are not supported without a file upload flow"
                            .to_string(),
                    )),
                }
            }
            ContentBlock::ToolUse { name, input, .. } => Ok(GeminiPart::FunctionCall {
                function_call: GeminiFunctionCall {
                    name: name.clone(),
                    args: input.clone(),
                },
            }),
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let name = tool_name_by_id.get(tool_use_id).cloned().ok_or_else(|| {
                    ProviderError::InvalidRequest(format!(
                        "Gemini tool result references unknown tool_use_id '{tool_use_id}'"
                    ))
                })?;

                Ok(GeminiPart::FunctionResponse {
                    function_response: GeminiFunctionResponse {
                        name,
                        response: json!({
                            "content": content,
                            "is_error": is_error,
                        }),
                    },
                })
            }
        }
    }
}

#[derive(Serialize)]
struct GeminiInlineData {
    #[serde(rename = "mimeType")]
    mime_type: String,
    data: String,
}

#[derive(Serialize)]
struct GeminiFunctionCall {
    name: String,
    args: Value,
}

#[derive(Serialize)]
struct GeminiFunctionResponse {
    name: String,
    response: Value,
}

#[derive(Serialize)]
struct GeminiTool {
    #[serde(rename = "functionDeclarations")]
    function_declarations: Vec<GeminiFunctionDeclaration>,
}

#[derive(Serialize)]
struct GeminiFunctionDeclaration {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    parameters: Value,
}

impl From<&ToolSpec> for GeminiFunctionDeclaration {
    fn from(tool: &ToolSpec) -> Self {
        GeminiFunctionDeclaration {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.input_schema.clone(),
        }
    }
}

#[derive(Serialize)]
struct GeminiToolConfig {
    #[serde(rename = "functionCallingConfig")]
    function_calling_config: GeminiFunctionCallingConfig,
}

impl From<ToolChoice> for GeminiToolConfig {
    fn from(choice: ToolChoice) -> Self {
        let function_calling_config = match choice {
            ToolChoice::Auto => GeminiFunctionCallingConfig {
                mode: GeminiFunctionCallingMode::Auto,
                allowed_function_names: Vec::new(),
            },
            ToolChoice::Any => GeminiFunctionCallingConfig {
                mode: GeminiFunctionCallingMode::Any,
                allowed_function_names: Vec::new(),
            },
            ToolChoice::Tool { name } => GeminiFunctionCallingConfig {
                mode: GeminiFunctionCallingMode::Any,
                allowed_function_names: vec![name],
            },
        };

        GeminiToolConfig {
            function_calling_config,
        }
    }
}

#[derive(Serialize)]
struct GeminiFunctionCallingConfig {
    mode: GeminiFunctionCallingMode,
    #[serde(rename = "allowedFunctionNames", skip_serializing_if = "Vec::is_empty")]
    allowed_function_names: Vec<String>,
}

#[derive(Serialize)]
enum GeminiFunctionCallingMode {
    #[serde(rename = "AUTO")]
    Auto,
    #[serde(rename = "ANY")]
    Any,
}

#[derive(Serialize)]
struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(rename = "maxOutputTokens", skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
}

impl GeminiGenerationConfig {
    fn from_request(request: &Request<'_>) -> Option<Self> {
        let config = GeminiGenerationConfig {
            temperature: request.temperature,
            max_output_tokens: request.max_output_tokens,
        };

        (!config.is_empty()).then_some(config)
    }

    fn is_empty(&self) -> bool {
        self.temperature.is_none() && self.max_output_tokens.is_none()
    }
}

#[cfg(test)]
mod tests {
    use std::{borrow::Cow, collections::BTreeMap};

    use serde_json::json;

    use crate::{
        ProviderId,
        provider::model::{ContentBlock, Message, ProviderError, Request, Role, ToolChoice},
        tool::ToolSpec,
    };

    use super::{GeminiGenerateContentRequest, GeminiModel};

    #[test]
    fn converts_request_to_gemini_payload() {
        let request = Request {
            model: Cow::Borrowed("gemini-2.0-flash"),
            system: Some(Cow::Borrowed("Be helpful.")),
            messages: Cow::Owned(vec![
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::text("What files changed?")],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "call_1".to_string(),
                        name: "read_file".to_string(),
                        input: json!({ "path": "README.md" }),
                    }],
                },
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "call_1".to_string(),
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
                capabilities: vec![],
                side_effect_level: crate::tool::ToolSideEffectLevel::None,
                durability: crate::tool::ToolDurability::ReplaySafe,
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

        let payload =
            serde_json::to_value(GeminiGenerateContentRequest::try_from(request).unwrap())
                .expect("request should serialize");

        assert_eq!(
            payload["systemInstruction"]["parts"][0]["text"],
            "Be helpful."
        );
        assert_eq!(payload["contents"][0]["role"], "user");
        assert_eq!(
            payload["contents"][0]["parts"][0]["text"],
            "What files changed?"
        );
        assert_eq!(
            payload["contents"][1]["parts"][0]["functionCall"]["name"],
            "read_file"
        );
        assert_eq!(
            payload["contents"][2]["parts"][0]["functionResponse"]["name"],
            "read_file"
        );
        assert_eq!(
            payload["contents"][2]["parts"][0]["functionResponse"]["response"]["content"],
            "README contents"
        );
        assert_eq!(
            payload["tools"][0]["functionDeclarations"][0]["name"],
            "read_file"
        );
        assert_eq!(
            payload["toolConfig"]["functionCallingConfig"]["mode"],
            "ANY"
        );
        assert_eq!(
            payload["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"][0],
            "read_file"
        );
        let temperature = payload["generationConfig"]["temperature"]
            .as_f64()
            .expect("temperature should be numeric");
        assert!((temperature - 0.2).abs() < 1e-6);
        assert_eq!(payload["generationConfig"]["maxOutputTokens"], 256);
        assert!(payload.get("metadata").is_none());
    }

    #[test]
    fn serializes_inline_images_into_inline_data_parts() {
        let request = Request {
            model: Cow::Borrowed("gemini-2.0-flash"),
            system: None,
            messages: Cow::Owned(vec![Message {
                role: Role::User,
                content: vec![
                    ContentBlock::text("Describe this"),
                    ContentBlock::image_bytes("image/png", [1_u8, 2, 3]),
                ],
            }]),
            tools: Cow::Owned(vec![]),
            tool_choice: Some(ToolChoice::Auto),
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
        };

        let payload =
            serde_json::to_value(GeminiGenerateContentRequest::try_from(request).unwrap())
                .expect("request should serialize");

        assert_eq!(payload["contents"][0]["parts"][0]["text"], "Describe this");
        assert_eq!(
            payload["contents"][0]["parts"][1]["inlineData"]["mimeType"],
            "image/png"
        );
        assert_eq!(
            payload["contents"][0]["parts"][1]["inlineData"]["data"],
            "AQID"
        );
    }

    #[test]
    fn rejects_url_images() {
        let request = Request {
            model: Cow::Borrowed("gemini-2.0-flash"),
            system: None,
            messages: Cow::Owned(vec![Message {
                role: Role::User,
                content: vec![ContentBlock::image_url("https://example.com/image.png")],
            }]),
            tools: Cow::Owned(vec![]),
            tool_choice: None,
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
        };

        let error = GeminiGenerateContentRequest::try_from(request)
            .err()
            .expect("request should fail");
        match error {
            ProviderError::InvalidRequest(message) => {
                assert!(message.contains("image URL inputs are not supported"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn serializes_tool_choice_modes() {
        let request = Request {
            model: Cow::Borrowed("gemini-2.0-flash"),
            system: None,
            messages: Cow::Owned(vec![Message {
                role: Role::User,
                content: vec![ContentBlock::text("hi")],
            }]),
            tools: Cow::Owned(vec![ToolSpec {
                name: "echo".to_string(),
                description: None,
                input_schema: json!({"type":"object"}),
                capabilities: vec![],
                side_effect_level: crate::tool::ToolSideEffectLevel::None,
                durability: crate::tool::ToolDurability::ReplaySafe,
            }]),
            tool_choice: Some(ToolChoice::Any),
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
        };
        let any_payload =
            serde_json::to_value(GeminiGenerateContentRequest::try_from(request).unwrap())
                .expect("request should serialize");
        assert_eq!(
            any_payload["toolConfig"]["functionCallingConfig"]["mode"],
            "ANY"
        );

        let request = Request {
            model: Cow::Borrowed("gemini-2.0-flash"),
            system: None,
            messages: Cow::Owned(vec![Message {
                role: Role::User,
                content: vec![ContentBlock::text("hi")],
            }]),
            tools: Cow::Owned(vec![ToolSpec {
                name: "echo".to_string(),
                description: None,
                input_schema: json!({"type":"object"}),
                capabilities: vec![],
                side_effect_level: crate::tool::ToolSideEffectLevel::None,
                durability: crate::tool::ToolDurability::ReplaySafe,
            }]),
            tool_choice: Some(ToolChoice::Auto),
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
        };
        let auto_payload =
            serde_json::to_value(GeminiGenerateContentRequest::try_from(request).unwrap())
                .expect("request should serialize");
        assert_eq!(
            auto_payload["toolConfig"]["functionCallingConfig"]["mode"],
            "AUTO"
        );
    }

    #[test]
    fn omits_tool_config_when_tool_choice_is_unset() {
        let request = Request {
            model: Cow::Borrowed("gemini-2.0-flash"),
            system: None,
            messages: Cow::Owned(vec![Message {
                role: Role::User,
                content: vec![ContentBlock::text("hi")],
            }]),
            tools: Cow::Owned(vec![ToolSpec {
                name: "echo".to_string(),
                description: None,
                input_schema: json!({"type":"object"}),
                capabilities: vec![],
                side_effect_level: crate::tool::ToolSideEffectLevel::None,
                durability: crate::tool::ToolDurability::ReplaySafe,
            }]),
            tool_choice: None,
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
        };

        let payload =
            serde_json::to_value(GeminiGenerateContentRequest::try_from(request).unwrap())
                .expect("request should serialize");

        assert!(payload.get("toolConfig").is_none());
    }

    #[test]
    fn converts_models_to_model_info() {
        let model = GeminiModel {
            name: "models/gemini-2.0-flash".to_string(),
            base_model_id: None,
            display_name: Some("Gemini 2.0 Flash".to_string()),
            description: Some("Fast Gemini model".to_string()),
            supported_generation_methods: vec!["generateContent".to_string()],
        };

        let info = crate::provider::model::ModelInfo::from(model);
        assert_eq!(info.id, "gemini-2.0-flash");
        assert_eq!(info.provider, ProviderId::GEMINI);
        assert_eq!(info.display_name.as_deref(), Some("Gemini 2.0 Flash"));
        assert_eq!(info.description.as_deref(), Some("Fast Gemini model"));
        assert_eq!(info.created_at, None);
    }
}
