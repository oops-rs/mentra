use crate::{
    ContentBlock, ImageSource, error::RuntimeError, provider::ContentBlockStart,
};
use mentra_provider::{
    ImageGenerationCall, ImageGenerationResult, HostedToolSearchCall, HostedWebSearchCall,
    ToolResultContent, WebSearchAction,
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
        content: ToolResultContent,
        is_error: bool,
        complete: bool,
    },
    HostedToolSearch {
        call: HostedToolSearchCall,
        complete: bool,
    },
    HostedWebSearch {
        call: HostedWebSearchCall,
        complete: bool,
    },
    ImageGeneration {
        call: ImageGenerationCall,
        complete: bool,
    },
}

impl PendingContentBlock {
    pub(super) fn is_complete(&self) -> bool {
        match self {
            PendingContentBlock::Text { complete, .. }
            | PendingContentBlock::Image { complete, .. }
            | PendingContentBlock::ToolUse { complete, .. }
            | PendingContentBlock::ToolResult { complete, .. }
            | PendingContentBlock::HostedToolSearch { complete, .. }
            | PendingContentBlock::HostedWebSearch { complete, .. }
            | PendingContentBlock::ImageGeneration { complete, .. } => *complete,
        }
    }

    pub(super) fn mark_complete(&mut self) {
        match self {
            PendingContentBlock::Text { complete, .. }
            | PendingContentBlock::Image { complete, .. }
            | PendingContentBlock::ToolUse { complete, .. }
            | PendingContentBlock::ToolResult { complete, .. }
            | PendingContentBlock::HostedToolSearch { complete, .. }
            | PendingContentBlock::HostedWebSearch { complete, .. }
            | PendingContentBlock::ImageGeneration { complete, .. } => *complete = true,
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
            PendingContentBlock::HostedToolSearch { call, .. } => {
                Ok(ContentBlock::HostedToolSearch { call: call.clone() })
            }
            PendingContentBlock::HostedWebSearch { call, .. } => {
                Ok(ContentBlock::HostedWebSearch { call: call.clone() })
            }
            PendingContentBlock::ImageGeneration { call, .. } => {
                Ok(ContentBlock::ImageGeneration { call: call.clone() })
            }
        }
    }

    pub(super) fn kind_name(&self) -> &'static str {
        match self {
            PendingContentBlock::Text { .. } => "text",
            PendingContentBlock::Image { .. } => "image",
            PendingContentBlock::ToolUse { .. } => "tool_use",
            PendingContentBlock::ToolResult { .. } => "tool_result",
            PendingContentBlock::HostedToolSearch { .. } => "hosted_tool_search",
            PendingContentBlock::HostedWebSearch { .. } => "hosted_web_search",
            PendingContentBlock::ImageGeneration { .. } => "image_generation",
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
                content,
                ..
            } => PendingContentBlock::ToolResult {
                tool_use_id,
                content: content.unwrap_or_default(),
                is_error,
                complete: false,
            },
            ContentBlockStart::HostedToolSearch { call } => PendingContentBlock::HostedToolSearch {
                call,
                complete: false,
            },
            ContentBlockStart::HostedWebSearch { call } => PendingContentBlock::HostedWebSearch {
                call,
                complete: false,
            },
            ContentBlockStart::ImageGeneration { call } => PendingContentBlock::ImageGeneration {
                call,
                complete: false,
            },
        }
    }
}

impl PendingContentBlock {
    pub(super) fn apply_hosted_delta(
        &mut self,
        delta: &crate::provider::ContentBlockDelta,
    ) -> bool {
        match (self, delta) {
            (
                PendingContentBlock::HostedToolSearch { call, .. },
                crate::provider::ContentBlockDelta::HostedToolSearchQuery(query),
            ) => {
                call.query = Some(query.clone());
                true
            }
            (
                PendingContentBlock::HostedToolSearch { call, .. },
                crate::provider::ContentBlockDelta::HostedToolSearchStatus(status),
            ) => {
                call.status = Some(status.clone());
                true
            }
            (
                PendingContentBlock::HostedWebSearch { call, .. },
                crate::provider::ContentBlockDelta::HostedWebSearchAction(action),
            ) => {
                call.action = Some(match action {
                    WebSearchAction::Search { query, queries } => WebSearchAction::Search {
                        query: query.clone(),
                        queries: queries.clone(),
                    },
                    WebSearchAction::OpenPage { url } => WebSearchAction::OpenPage {
                        url: url.clone(),
                    },
                    WebSearchAction::FindInPage { url, pattern } => WebSearchAction::FindInPage {
                        url: url.clone(),
                        pattern: pattern.clone(),
                    },
                });
                true
            }
            (
                PendingContentBlock::HostedWebSearch { call, .. },
                crate::provider::ContentBlockDelta::HostedWebSearchStatus(status),
            ) => {
                call.status = Some(status.clone());
                true
            }
            (
                PendingContentBlock::ImageGeneration { call, .. },
                crate::provider::ContentBlockDelta::ImageGenerationStatus(status),
            ) => {
                call.status = status.clone();
                true
            }
            (
                PendingContentBlock::ImageGeneration { call, .. },
                crate::provider::ContentBlockDelta::ImageGenerationRevisedPrompt(prompt),
            ) => {
                call.revised_prompt = Some(prompt.clone());
                true
            }
            (
                PendingContentBlock::ImageGeneration { call, .. },
                crate::provider::ContentBlockDelta::ImageGenerationResult(result),
            ) => {
                call.result = Some(match result {
                    ImageGenerationResult::Image { source } => ImageGenerationResult::Image {
                        source: source.clone(),
                    },
                    ImageGenerationResult::ArtifactRef { artifact_id } => {
                        ImageGenerationResult::ArtifactRef {
                            artifact_id: artifact_id.clone(),
                        }
                    }
                });
                true
            }
            _ => false,
        }
    }
}
