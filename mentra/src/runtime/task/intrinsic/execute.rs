use crate::{
    ContentBlock,
    runtime::Agent,
    tool::{ToolCall, ToolContext, ToolResult, internal::content_block_to_tool_result},
};
use strum::VariantArray;

use super::{TaskIntrinsicTool, descriptor::task_intrinsic_descriptor};

pub(super) fn execute_mut(
    tool: TaskIntrinsicTool,
    ctx: ToolContext<'_>,
    input: serde_json::Value,
) -> ToolResult {
    let call = ToolCall {
        id: ctx.tool_call_id.clone(),
        name: task_intrinsic_descriptor(tool).provider.name,
        input,
    };
    let Some(result) = execute_intrinsic(ctx.agent, call) else {
        return Err("Task intrinsic is not available".to_string());
    };
    content_block_to_tool_result("Task intrinsic", result)
}

pub(super) fn execute_intrinsic(agent: &mut Agent, call: ToolCall) -> Option<ContentBlock> {
    let tool = TaskIntrinsicTool::VARIANTS
        .iter()
        .find(|tool| task_intrinsic_descriptor(**tool).provider.name == call.name)?;

    let output = agent.execute_task_mutation(tool, call.input);

    Some(match output {
        Ok(content) => match agent.refresh_tasks_from_disk() {
            Ok(()) => ContentBlock::ToolResult {
                tool_use_id: call.id,
                content: content.into(),
                is_error: false,
            },
            Err(error) => ContentBlock::ToolResult {
                tool_use_id: call.id,
                content: format!("Task refresh failed: {error}").into(),
                is_error: true,
            },
        },
        Err(content) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: content.into(),
            is_error: true,
        },
    })
}
