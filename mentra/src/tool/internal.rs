use serde_json::Value;

use crate::ContentBlock;

use super::{
    RuntimeToolDescriptor, ToolApprovalCategory, ToolCapability, ToolDurability,
    ToolExecutionCategory, ToolResult, ToolSideEffectLevel,
};

pub(crate) struct RuntimeDescriptorParts {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) input_schema: Value,
    pub(crate) capabilities: Vec<ToolCapability>,
    pub(crate) side_effect_level: ToolSideEffectLevel,
    pub(crate) durability: ToolDurability,
    pub(crate) execution_category: ToolExecutionCategory,
    pub(crate) approval_category: ToolApprovalCategory,
}

pub(crate) fn build_runtime_descriptor(parts: RuntimeDescriptorParts) -> RuntimeToolDescriptor {
    RuntimeToolDescriptor::builder(parts.name)
        .description(parts.description)
        .input_schema(parts.input_schema)
        .capabilities(parts.capabilities)
        .side_effect_level(parts.side_effect_level)
        .durability(parts.durability)
        .execution_category(parts.execution_category)
        .approval_category(parts.approval_category)
        .build()
}

pub(crate) fn content_block_to_tool_result(surface: &str, block: ContentBlock) -> ToolResult {
    match block {
        ContentBlock::ToolResult {
            content, is_error, ..
        } => {
            if is_error {
                Err(content.to_display_string())
            } else {
                Ok(content.to_display_string())
            }
        }
        _ => Err(format!("{surface} returned an unexpected content block")),
    }
}
