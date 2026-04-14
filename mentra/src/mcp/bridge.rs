//! Bridge that wraps MCP server tools as Mentra `ExecutableTool` instances.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::tool::{
    ParallelToolContext, RuntimeToolDescriptor, ToolApprovalCategory, ToolCapability,
    ToolDefinition, ToolDurability, ToolExecutionCategory, ToolExecutor, ToolResult,
    ToolSideEffectLevel,
};

use super::client::McpStdioClient;
use super::protocol::McpToolDefinition;

/// Prefix applied to MCP tool names to namespace them.
const MCP_TOOL_PREFIX: &str = "mcp__";

/// Construct the namespaced tool name for an MCP tool.
pub fn mcp_tool_name(server_name: &str, tool_name: &str) -> String {
    format!("{MCP_TOOL_PREFIX}{server_name}__{tool_name}")
}

/// Parse a namespaced MCP tool name back into `(server_name, tool_name)`.
pub fn parse_mcp_tool_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix(MCP_TOOL_PREFIX)?;
    let (server, tool) = rest.split_once("__")?;
    Some((server, tool))
}

/// A Mentra tool backed by an MCP server tool.
pub struct McpBridgedTool {
    server_name: String,
    tool_def: McpToolDefinition,
    client: Arc<McpStdioClient>,
}

impl McpBridgedTool {
    pub fn new(
        server_name: String,
        tool_def: McpToolDefinition,
        client: Arc<McpStdioClient>,
    ) -> Self {
        Self {
            server_name,
            tool_def,
            client,
        }
    }

    fn full_name(&self) -> String {
        mcp_tool_name(&self.server_name, &self.tool_def.name)
    }
}

impl ToolDefinition for McpBridgedTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        let description = self.tool_def.description.clone().unwrap_or_default();

        let input_schema = self
            .tool_def
            .input_schema
            .clone()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));

        RuntimeToolDescriptor::builder(self.full_name())
            .description(description)
            .input_schema(input_schema)
            .capability(ToolCapability::Custom(format!("mcp:{}", self.server_name)))
            .side_effect_level(ToolSideEffectLevel::External)
            .durability(ToolDurability::Ephemeral)
            .execution_category(ToolExecutionCategory::ExclusiveLocalMutation)
            .approval_category(ToolApprovalCategory::Process)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for McpBridgedTool {
    async fn execute(&self, _ctx: ParallelToolContext, input: Value) -> ToolResult {
        let arguments = if input.is_null()
            || (input.is_object() && input.as_object().is_none_or(|o| o.is_empty()))
        {
            None
        } else {
            Some(input)
        };

        let result = self
            .client
            .call_tool(&self.tool_def.name, arguments)
            .await
            .map_err(|e| format!("MCP tool call failed: {e}"))?;

        // Concatenate text content blocks into the result string.
        let mut output = String::new();
        for block in &result.content {
            if let Some(text) = &block.text {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(text);
            }
        }

        if result.is_error {
            Err(output)
        } else {
            Ok(output)
        }
    }
}
