use serde_json::json;

use crate::tool::{
    RuntimeToolDescriptor, ToolApprovalCategory, ToolCapability, ToolDurability,
    ToolExecutionCategory, ToolSideEffectLevel,
    internal::{RuntimeDescriptorParts, build_runtime_descriptor},
};

use super::RuntimeIntrinsicTool;

pub(super) fn runtime_intrinsic_descriptor(tool: RuntimeIntrinsicTool) -> RuntimeToolDescriptor {
    match tool {
        RuntimeIntrinsicTool::Compact => build_runtime_descriptor(RuntimeDescriptorParts {
            name: tool.to_string(),
            description: "Compress older conversation context into a summary.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
            capabilities: vec![ToolCapability::ContextCompaction],
            side_effect_level: ToolSideEffectLevel::LocalState,
            durability: ToolDurability::Persistent,
            execution_category: ToolExecutionCategory::ExclusivePersistentMutation,
            approval_category: ToolApprovalCategory::Default,
        }),
        RuntimeIntrinsicTool::Idle => build_runtime_descriptor(RuntimeDescriptorParts {
            name: tool.to_string(),
            description: "Yield the current turn and return to the teammate idle loop.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
            capabilities: vec![ToolCapability::Delegation],
            side_effect_level: ToolSideEffectLevel::LocalState,
            durability: ToolDurability::Persistent,
            execution_category: ToolExecutionCategory::Delegation,
            approval_category: ToolApprovalCategory::Delegation,
        }),
        RuntimeIntrinsicTool::MemorySearch => build_runtime_descriptor(RuntimeDescriptorParts {
            name: tool.to_string(),
            description: "Search the current agent's long-term memory for additional recall."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Memory query text"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results to return"
                    }
                },
                "required": ["query"]
            }),
            capabilities: vec![ToolCapability::ReadOnly],
            side_effect_level: ToolSideEffectLevel::None,
            durability: ToolDurability::ReplaySafe,
            execution_category: ToolExecutionCategory::ReadOnlyParallel,
            approval_category: ToolApprovalCategory::ReadOnly,
        }),
        RuntimeIntrinsicTool::MemoryPin => build_runtime_descriptor(RuntimeDescriptorParts {
            name: tool.to_string(),
            description: "Persist a fact in long-term memory for the current agent.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "Fact to remember"
                    }
                },
                "required": ["content"]
            }),
            capabilities: vec![ToolCapability::Custom("memory_write".to_string())],
            side_effect_level: ToolSideEffectLevel::LocalState,
            durability: ToolDurability::Persistent,
            execution_category: ToolExecutionCategory::ExclusivePersistentMutation,
            approval_category: ToolApprovalCategory::Default,
        }),
        RuntimeIntrinsicTool::MemoryForget => build_runtime_descriptor(RuntimeDescriptorParts {
            name: tool.to_string(),
            description: "Forget a specific long-term memory record by id.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "record_id": {
                        "type": "string",
                        "description": "Identifier of the memory record to forget"
                    }
                },
                "required": ["record_id"]
            }),
            capabilities: vec![ToolCapability::Custom("memory_write".to_string())],
            side_effect_level: ToolSideEffectLevel::LocalState,
            durability: ToolDurability::Persistent,
            execution_category: ToolExecutionCategory::ExclusivePersistentMutation,
            approval_category: ToolApprovalCategory::Default,
        }),
        RuntimeIntrinsicTool::Task => build_runtime_descriptor(RuntimeDescriptorParts {
            name: tool.to_string(),
            description: "Spawn a fresh subagent to work a subtask and return a concise summary."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "Delegated task prompt for the subagent"
                    }
                },
                "required": ["prompt"]
            }),
            capabilities: vec![ToolCapability::Delegation],
            side_effect_level: ToolSideEffectLevel::LocalState,
            durability: ToolDurability::Ephemeral,
            execution_category: ToolExecutionCategory::Delegation,
            approval_category: ToolApprovalCategory::Delegation,
        }),
    }
}
