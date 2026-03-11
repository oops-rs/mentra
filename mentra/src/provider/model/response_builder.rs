use std::collections::BTreeMap;

use super::{
    ContentBlock, ContentBlockDelta, ContentBlockStart, ProviderError, ProviderEvent,
    ProviderEventStream, Response, Role,
};

pub async fn collect_response_from_stream(
    mut stream: ProviderEventStream,
) -> Result<Response, ProviderError> {
    let mut builder = StreamingResponseBuilder::default();

    while let Some(event) = stream.recv().await {
        builder.apply(event?)?;
    }

    builder.build()
}

#[derive(Default)]
struct StreamingResponseBuilder {
    id: Option<String>,
    model: Option<String>,
    role: Option<Role>,
    blocks: BTreeMap<usize, StreamingContentBlock>,
    stop_reason: Option<String>,
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
            ProviderEvent::MessageDelta { stop_reason } => {
                self.stop_reason = stop_reason;
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
        })
    }
}

enum StreamingContentBlock {
    Text {
        text: String,
        complete: bool,
    },
    Image {
        source: super::ImageSource,
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
