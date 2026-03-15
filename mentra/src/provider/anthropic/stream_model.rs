use serde::Deserialize;

use crate::provider::model::{ContentBlockDelta, ContentBlockStart, ProviderEvent, Role};

use super::model::{AnthropicResponse, AnthropicUsage};

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum AnthropicStreamEvent {
    MessageStart {
        message: AnthropicResponse,
    },
    ContentBlockStart {
        index: usize,
        content_block: AnthropicStreamContentBlock,
    },
    ContentBlockDelta {
        index: usize,
        delta: AnthropicContentBlockDelta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: AnthropicMessageDelta,
    },
    MessageStop,
    Ping,
    Error {
        error: AnthropicStreamError,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum AnthropicStreamContentBlock {
    Text {},
    ToolUse {
        id: String,
        name: String,
    },
    #[serde(other)]
    Unsupported,
}

impl AnthropicStreamContentBlock {
    pub(crate) fn into_provider_start(self) -> Option<ContentBlockStart> {
        match self {
            AnthropicStreamContentBlock::Text {} => Some(ContentBlockStart::Text),
            AnthropicStreamContentBlock::ToolUse { id, name } => {
                Some(ContentBlockStart::ToolUse { id, name })
            }
            AnthropicStreamContentBlock::Unsupported => None,
        }
    }

    pub(crate) fn is_supported(&self) -> bool {
        !matches!(self, AnthropicStreamContentBlock::Unsupported)
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum AnthropicContentBlockDelta {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    #[serde(other)]
    Unsupported,
}

impl AnthropicContentBlockDelta {
    pub(crate) fn into_provider_delta(self) -> Option<ContentBlockDelta> {
        match self {
            AnthropicContentBlockDelta::TextDelta { text } => Some(ContentBlockDelta::Text(text)),
            AnthropicContentBlockDelta::InputJsonDelta { partial_json } => {
                Some(ContentBlockDelta::ToolUseInputJson(partial_json))
            }
            AnthropicContentBlockDelta::Unsupported => None,
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct AnthropicMessageDelta {
    pub(crate) stop_reason: Option<String>,
    #[serde(default)]
    pub(crate) usage: Option<AnthropicUsage>,
}

#[derive(Deserialize)]
pub(crate) struct AnthropicStreamError {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) message: String,
}

impl AnthropicStreamEvent {
    pub(crate) fn into_provider_events(self) -> Result<Vec<ProviderEvent>, AnthropicStreamError> {
        match self {
            AnthropicStreamEvent::MessageStart { message } => {
                let usage = message
                    .usage
                    .clone()
                    .and_then(AnthropicUsage::into_token_usage);
                let mut events = vec![ProviderEvent::MessageStarted {
                    id: message.id,
                    model: message.model,
                    role: match message.role.as_str() {
                        "user" => Role::User,
                        "assistant" => Role::Assistant,
                        _ => Role::Unknown(message.role),
                    },
                }];
                if let Some(usage) = usage {
                    events.push(ProviderEvent::MessageDelta {
                        stop_reason: None,
                        usage: Some(usage),
                    });
                }
                Ok(events)
            }
            AnthropicStreamEvent::ContentBlockStart {
                index,
                content_block,
            } => Ok(content_block
                .into_provider_start()
                .map(|kind| vec![ProviderEvent::ContentBlockStarted { index, kind }])
                .unwrap_or_default()),
            AnthropicStreamEvent::ContentBlockDelta { index, delta } => Ok(delta
                .into_provider_delta()
                .map(|delta| vec![ProviderEvent::ContentBlockDelta { index, delta }])
                .unwrap_or_default()),
            AnthropicStreamEvent::ContentBlockStop { index } => {
                Ok(vec![ProviderEvent::ContentBlockStopped { index }])
            }
            AnthropicStreamEvent::MessageDelta { delta } => Ok(vec![ProviderEvent::MessageDelta {
                stop_reason: delta.stop_reason,
                usage: delta.usage.and_then(AnthropicUsage::into_token_usage),
            }]),
            AnthropicStreamEvent::MessageStop => Ok(vec![ProviderEvent::MessageStopped]),
            AnthropicStreamEvent::Ping => Ok(Vec::new()),
            AnthropicStreamEvent::Error { error } => Err(error),
        }
    }
}
