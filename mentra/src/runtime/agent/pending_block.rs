use crate::{
    provider::ContentBlockStart,
    ContentBlock, ImageSource,
    runtime::error::RuntimeError,
};

use super::PendingToolUseSummary;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PendingContentBlock {
    Text {
        text: String,
        complete: bool,
    },
    Image {
        source: ImageSource,
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

impl PendingContentBlock {
    pub(super) fn is_complete(&self) -> bool {
        match self {
            PendingContentBlock::Text { complete, .. }
            | PendingContentBlock::Image { complete, .. }
            | PendingContentBlock::ToolUse { complete, .. }
            | PendingContentBlock::ToolResult { complete, .. } => *complete,
        }
    }

    pub(super) fn mark_complete(&mut self) {
        match self {
            PendingContentBlock::Text { complete, .. }
            | PendingContentBlock::Image { complete, .. }
            | PendingContentBlock::ToolUse { complete, .. }
            | PendingContentBlock::ToolResult { complete, .. } => *complete = true,
        }
    }

    pub(super) fn to_content_block(&self) -> Result<ContentBlock, RuntimeError> {
        match self {
            PendingContentBlock::Text { text, .. } => Ok(ContentBlock::Text { text: text.clone() }),
            PendingContentBlock::Image { source, .. } => Ok(ContentBlock::Image {
                source: source.clone(),
            }),
            PendingContentBlock::ToolUse {
                id,
                name,
                input_json,
                ..
            } => Ok(ContentBlock::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: serde_json::from_str(input_json).map_err(|source| {
                    RuntimeError::InvalidToolUseInput {
                        id: id.clone(),
                        name: name.clone(),
                        source,
                    }
                })?,
            }),
            PendingContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => Ok(ContentBlock::ToolResult {
                tool_use_id: tool_use_id.clone(),
                content: content.clone(),
                is_error: *is_error,
            }),
        }
    }

    pub(super) fn kind_name(&self) -> &'static str {
        match self {
            PendingContentBlock::Text { .. } => "text",
            PendingContentBlock::Image { .. } => "image",
            PendingContentBlock::ToolUse { .. } => "tool_use",
            PendingContentBlock::ToolResult { .. } => "tool_result",
        }
    }

    pub(super) fn tool_use_summary(&self) -> Option<PendingToolUseSummary> {
        match self {
            PendingContentBlock::ToolUse {
                id,
                name,
                input_json,
                complete,
            } => Some(PendingToolUseSummary {
                id: id.clone(),
                name: name.clone(),
                input_json: input_json.clone(),
                complete: *complete,
            }),
            _ => None,
        }
    }
}

impl From<ContentBlockStart> for PendingContentBlock {
    fn from(value: ContentBlockStart) -> Self {
        match value {
            ContentBlockStart::Text => PendingContentBlock::Text {
                text: String::new(),
                complete: false,
            },
            ContentBlockStart::Image { source } => PendingContentBlock::Image {
                source,
                complete: false,
            },
            ContentBlockStart::ToolUse { id, name } => PendingContentBlock::ToolUse {
                id,
                name,
                input_json: String::new(),
                complete: false,
            },
            ContentBlockStart::ToolResult {
                tool_use_id,
                is_error,
            } => PendingContentBlock::ToolResult {
                tool_use_id,
                content: String::new(),
                is_error,
                complete: false,
            },
        }
    }
}
