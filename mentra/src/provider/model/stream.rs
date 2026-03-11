use tokio::sync::mpsc;

use super::{ContentBlock, ImageSource, ProviderError, Response, Role};

pub type ProviderEventStream = mpsc::UnboundedReceiver<Result<ProviderEvent, ProviderError>>;

#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    MessageStarted {
        id: String,
        model: String,
        role: Role,
    },
    ContentBlockStarted {
        index: usize,
        kind: ContentBlockStart,
    },
    ContentBlockDelta {
        index: usize,
        delta: ContentBlockDelta,
    },
    ContentBlockStopped {
        index: usize,
    },
    MessageDelta {
        stop_reason: Option<String>,
    },
    MessageStopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentBlockStart {
    Text,
    Image { source: ImageSource },
    ToolUse { id: String, name: String },
    ToolResult { tool_use_id: String, is_error: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentBlockDelta {
    Text(String),
    ToolUseInputJson(String),
    ToolResultContent(String),
}

pub fn provider_event_stream_from_response(response: Response) -> ProviderEventStream {
    let events = response.into_provider_events();
    let (tx, rx) = mpsc::unbounded_channel();

    for event in events {
        if tx.send(Ok(event)).is_err() {
            break;
        }
    }

    rx
}

impl Response {
    pub fn into_provider_events(self) -> Vec<ProviderEvent> {
        let mut events = vec![ProviderEvent::MessageStarted {
            id: self.id,
            model: self.model,
            role: self.role,
        }];

        for (index, block) in self.content.into_iter().enumerate() {
            events.extend(block.into_provider_events(index));
        }

        events.push(ProviderEvent::MessageDelta {
            stop_reason: self.stop_reason,
        });
        events.push(ProviderEvent::MessageStopped);
        events
    }
}

impl ContentBlock {
    fn into_provider_events(self, index: usize) -> Vec<ProviderEvent> {
        match self {
            ContentBlock::Text { text } => {
                let mut events = vec![ProviderEvent::ContentBlockStarted {
                    index,
                    kind: ContentBlockStart::Text,
                }];
                if !text.is_empty() {
                    events.push(ProviderEvent::ContentBlockDelta {
                        index,
                        delta: ContentBlockDelta::Text(text),
                    });
                }
                events.push(ProviderEvent::ContentBlockStopped { index });
                events
            }
            ContentBlock::Image { source } => vec![
                ProviderEvent::ContentBlockStarted {
                    index,
                    kind: ContentBlockStart::Image { source },
                },
                ProviderEvent::ContentBlockStopped { index },
            ],
            ContentBlock::ToolUse { id, name, input } => {
                let mut events = vec![ProviderEvent::ContentBlockStarted {
                    index,
                    kind: ContentBlockStart::ToolUse { id, name },
                }];
                let input_json = input.to_string();
                if !input_json.is_empty() {
                    events.push(ProviderEvent::ContentBlockDelta {
                        index,
                        delta: ContentBlockDelta::ToolUseInputJson(input_json),
                    });
                }
                events.push(ProviderEvent::ContentBlockStopped { index });
                events
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let mut events = vec![ProviderEvent::ContentBlockStarted {
                    index,
                    kind: ContentBlockStart::ToolResult {
                        tool_use_id,
                        is_error,
                    },
                }];
                if !content.is_empty() {
                    events.push(ProviderEvent::ContentBlockDelta {
                        index,
                        delta: ContentBlockDelta::ToolResultContent(content),
                    });
                }
                events.push(ProviderEvent::ContentBlockStopped { index });
                events
            }
        }
    }
}
