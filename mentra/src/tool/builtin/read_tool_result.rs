//! The `read_tool_result` built-in: reads further windows of a tool result
//! that was too large to deliver whole.
//!
//! It serves only the results this agent's own run retained, it never
//! re-executes the tool that produced them (so an expensive or
//! side-effectful call is not repeated to read more of its output), and its
//! own output is bounded by the agent's `page_bytes`, so reading can never
//! reintroduce the overflow paging exists to prevent.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tool::{
    ToolApprovalCategory, ToolCapability, ToolContext, ToolDefinition, ToolDurability,
    ToolExecutionCategory, ToolExecutor, ToolResult, ToolSideEffectLevel, ToolSpec,
    paging::{READ_TOOL_RESULT_TOOL, ToolResultPager},
};

pub(crate) struct ReadToolResultTool;

#[derive(Deserialize)]
struct ReadToolResultInput {
    tool_use_id: String,
    start_line: usize,
}

impl ToolDefinition for ReadToolResultTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder(READ_TOOL_RESULT_TOOL)
            .description(
                "Read the next window of a tool result that was too large to deliver whole. \
                 Pass the tool_use_id and start_line printed in that result's paging trailer. \
                 Line numbers are absolute over the full result, so they mean the same thing \
                 in every window. This reads retained output only — it never re-runs the tool \
                 that produced it.",
            )
            .input_schema(json!({
                "type": "object",
                "properties": {
                    "tool_use_id": {
                        "type": "string",
                        "description": "The tool_use_id printed in the paging trailer."
                    },
                    "start_line": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "1-based absolute line to start the window at."
                    }
                },
                "required": ["tool_use_id", "start_line"]
            }))
            .capability(ToolCapability::ReadOnly)
            .side_effect_level(ToolSideEffectLevel::None)
            .durability(ToolDurability::ReplaySafe)
            // Exclusive rather than ReadOnlyParallel despite reading nothing
            // but memory: the retained results are agent state, and only the
            // exclusive lane's `ToolContext` carries the agent.
            .execution_category(ToolExecutionCategory::ExclusiveLocalMutation)
            .approval_category(ToolApprovalCategory::ReadOnly)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for ReadToolResultTool {
    async fn execute_mut(&self, ctx: ToolContext<'_>, input: Value) -> ToolResult {
        let request: ReadToolResultInput = serde_json::from_value(input).map_err(|error| {
            format!(
                "{READ_TOOL_RESULT_TOOL} expects a tool_use_id string and a start_line integer \
                 of 1 or greater: {error}"
            )
        })?;
        if request.start_line == 0 {
            return Err(format!(
                "{READ_TOOL_RESULT_TOOL} start_line is 1-based; 0 is not a line"
            ));
        }

        let Some(paging) = ctx.tool_result_paging() else {
            return Err(format!(
                "{READ_TOOL_RESULT_TOOL} is unavailable: tool-result paging is not enabled \
                 for this agent"
            ));
        };
        let Some(full) = ctx.paged_tool_result(&request.tool_use_id) else {
            return Err(format!(
                "no retained result for tool_use_id \"{}\". Only results large enough to be \
                 paged are retained, and only for this agent's current run — use the \
                 tool_use_id printed in a paging trailer.",
                request.tool_use_id
            ));
        };

        Ok(ToolResultPager::new(paging).window(&request.tool_use_id, &full, request.start_line))
    }
}
