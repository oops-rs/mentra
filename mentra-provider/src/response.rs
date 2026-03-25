use serde::{Deserialize, Serialize};

use crate::{
    error::ProviderError,
    model::{ContentBlock, Role, TokenUsage},
    request::CompactionInputItem,
    stream::{ContentBlockDelta, ContentBlockStart, ProviderEvent, ProviderEventStream},
};

/// A complete response collected from a provider stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    pub model: String,
    pub role: Role,
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<String>,
    pub usage: Option<TokenUsage>,
}

/// A complete history-compaction response collected from a provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionResponse {
    pub output: Vec<CompactionInputItem>,
}

/// Rebuilds a full response from a provider event stream.
pub async fn collect_response_from_stream(
    mut stream: ProviderEventStream,
) -> Result<Response, ProviderError> {
    let mut builder = StreamingResponseBuilder::default();

    while let Some(event) = stream.recv().await {
        builder.apply(event?)?;
    }

    builder.build()
}

/// Converts a response into a provider event stream.
pub fn provider_event_stream_from_response(response: Response) -> ProviderEventStream {
    let events = response.into_provider_events();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

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
            usage: self.usage,
        });
        events.push(ProviderEvent::MessageStopped);
        events
    }
}

#[derive(Default)]
struct StreamingResponseBuilder {
    id: Option<String>,
    model: Option<String>,
    role: Option<Role>,
    blocks: std::collections::BTreeMap<usize, StreamingContentBlock>,
    stop_reason: Option<String>,
    usage: Option<TokenUsage>,
    stopped: bool,
}

impl StreamingResponseBuilder {
    fn apply(&mut self, event: ProviderEvent) -> Result<(), ProviderError> {
        match event {
            ProviderEvent::MessageStarted { id, model, role } => {
                self.id = Some(id);
                self.model = Some(model);
                self.role = Some(role);
            }
            ProviderEvent::ContentBlockStarted { index, kind } => {
                self.blocks.insert(index, StreamingContentBlock::from(kind));
            }
            ProviderEvent::ContentBlockDelta { index, delta } => {
                let block = self.blocks.get_mut(&index).ok_or_else(|| {
                    ProviderError::MalformedStream(format!(
                        "content block delta received before start for index {index}"
                    ))
                })?;
                block.apply_delta(delta)?;
            }
            ProviderEvent::ContentBlockStopped { index } => {
                let block = self.blocks.get_mut(&index).ok_or_else(|| {
                    ProviderError::MalformedStream(format!(
                        "content block stop received before start for index {index}"
                    ))
                })?;
                block.mark_complete();
            }
            ProviderEvent::MessageDelta { stop_reason, usage } => {
                self.stop_reason = stop_reason;
                self.usage = usage;
            }
            ProviderEvent::MessageStopped => {
                self.stopped = true;
            }
        }

        Ok(())
    }

    fn build(self) -> Result<Response, ProviderError> {
        if !self.stopped {
            return Err(ProviderError::MalformedStream(
                "message stream ended before MessageStopped".to_string(),
            ));
        }

        let id = self
            .id
            .ok_or_else(|| ProviderError::MalformedStream("missing message id".to_string()))?;
        let model = self
            .model
            .ok_or_else(|| ProviderError::MalformedStream("missing model id".to_string()))?;
        let role = self
            .role
            .ok_or_else(|| ProviderError::MalformedStream("missing message role".to_string()))?;
        let mut content = Vec::with_capacity(self.blocks.len());

        for (index, block) in self.blocks {
            if !block.is_complete() {
                return Err(ProviderError::MalformedStream(format!(
                    "content block {index} did not complete"
                )));
            }
            content.push(block.try_into_content_block()?);
        }

        Ok(Response {
            id,
            model,
            role,
            content,
            stop_reason: self.stop_reason,
            usage: self.usage,
        })
    }
}

enum StreamingContentBlock {
    Text {
        text: String,
        complete: bool,
    },
    Image {
        source: crate::model::ImageSource,
        complete: bool,
    },
    ToolUse {
        id: String,
        name: String,
        input_json: String,
        complete: bool,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
        complete: bool,
    },
}

impl StreamingContentBlock {
    fn apply_delta(&mut self, delta: ContentBlockDelta) -> Result<(), ProviderError> {
        match (self, delta) {
            (StreamingContentBlock::Text { text, .. }, ContentBlockDelta::Text(delta)) => {
                text.push_str(&delta);
                Ok(())
            }
            (
                StreamingContentBlock::ToolUse { input_json, .. },
                ContentBlockDelta::ToolUseInputJson(delta),
            ) => {
                input_json.push_str(&delta);
                Ok(())
            }
            (
                StreamingContentBlock::ToolResult { content, .. },
                ContentBlockDelta::ToolResultContent(delta),
            ) => {
                content.push_str(&delta);
                Ok(())
            }
            (block, delta) => Err(ProviderError::MalformedStream(format!(
                "delta {delta:?} is not valid for block {}",
                block.kind_name()
            ))),
        }
    }

    fn mark_complete(&mut self) {
        match self {
            StreamingContentBlock::Text { complete, .. }
            | StreamingContentBlock::Image { complete, .. }
            | StreamingContentBlock::ToolUse { complete, .. }
            | StreamingContentBlock::ToolResult { complete, .. } => *complete = true,
        }
    }

    fn is_complete(&self) -> bool {
        match self {
            StreamingContentBlock::Text { complete, .. }
            | StreamingContentBlock::Image { complete, .. }
            | StreamingContentBlock::ToolUse { complete, .. }
            | StreamingContentBlock::ToolResult { complete, .. } => *complete,
        }
    }

    fn try_into_content_block(self) -> Result<ContentBlock, ProviderError> {
        match self {
            StreamingContentBlock::Text { text, .. } => Ok(ContentBlock::Text { text }),
            StreamingContentBlock::Image { source, .. } => Ok(ContentBlock::Image { source }),
            StreamingContentBlock::ToolUse {
                id,
                name,
                input_json,
                ..
            } => Ok(ContentBlock::ToolUse {
                id,
                name,
                input: serde_json::from_str(&input_json).map_err(ProviderError::Deserialize)?,
            }),
            StreamingContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => Ok(ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            }),
        }
    }

    fn kind_name(&self) -> &'static str {
        match self {
            StreamingContentBlock::Text { .. } => "text",
            StreamingContentBlock::Image { .. } => "image",
            StreamingContentBlock::ToolUse { .. } => "tool_use",
            StreamingContentBlock::ToolResult { .. } => "tool_result",
        }
    }
}

impl From<ContentBlockStart> for StreamingContentBlock {
    fn from(value: ContentBlockStart) -> Self {
        match value {
            ContentBlockStart::Text => StreamingContentBlock::Text {
                text: String::new(),
                complete: false,
            },
            ContentBlockStart::Image { source } => StreamingContentBlock::Image {
                source,
                complete: false,
            },
            ContentBlockStart::ToolUse { id, name } => StreamingContentBlock::ToolUse {
                id,
                name,
                input_json: String::new(),
                complete: false,
            },
            ContentBlockStart::ToolResult {
                tool_use_id,
                is_error,
            } => StreamingContentBlock::ToolResult {
                tool_use_id,
                content: String::new(),
                is_error,
                complete: false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn response_round_trip_preserves_usage() {
        let response = Response {
            id: "resp-1".to_string(),
            model: "model".to_string(),
            role: Role::Assistant,
            content: vec![ContentBlock::text("hello")],
            stop_reason: Some("stop".to_string()),
            usage: Some(TokenUsage {
                input_tokens: Some(10),
                output_tokens: Some(3),
                total_tokens: Some(13),
                cache_read_input_tokens: Some(2),
                cache_creation_input_tokens: None,
                reasoning_tokens: Some(1),
                thoughts_tokens: None,
                tool_input_tokens: None,
            }),
        };

        let rebuilt =
            collect_response_from_stream(provider_event_stream_from_response(response.clone()))
                .await
                .expect("response should rebuild");

        assert_eq!(rebuilt, response);
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
