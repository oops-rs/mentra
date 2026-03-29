#[path = "intrinsic/descriptor.rs"]
mod descriptor;
#[path = "intrinsic/execute.rs"]
mod execute;

use async_trait::async_trait;
use strum::{Display, VariantArray};

use crate::tool::{
    ParallelToolContext, RuntimeToolDescriptor, ToolContext, ToolDefinition, ToolExecutor,
    ToolResult,
};

pub(crate) fn register_tools(registry: &mut crate::tool::ToolRegistry) {
    RuntimeIntrinsicTool::VARIANTS
        .iter()
        .for_each(|tool| registry.register_tool(*tool));
    crate::runtime::task::TaskIntrinsicTool::VARIANTS
        .iter()
        .for_each(|tool| registry.register_tool(*tool));
    crate::team::TeamIntrinsicTool::VARIANTS
        .iter()
        .for_each(|tool| registry.register_tool(*tool));
}

#[derive(Display, Copy, Clone, VariantArray)]
#[strum(serialize_all = "snake_case")]
pub(crate) enum RuntimeIntrinsicTool {
    Compact,
    Idle,
    MemoryForget,
    MemoryPin,
    MemorySearch,
    Task,
}

impl ToolDefinition for RuntimeIntrinsicTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        descriptor::runtime_intrinsic_descriptor(*self)
    }
}

#[async_trait]
impl ToolExecutor for RuntimeIntrinsicTool {
    async fn execute(&self, ctx: ParallelToolContext, input: serde_json::Value) -> ToolResult {
        execute::execute_parallel(*self, ctx, input).await
    }

    async fn execute_mut(&self, ctx: ToolContext<'_>, input: serde_json::Value) -> ToolResult {
        execute::execute_mut(*self, ctx, input).await
    }
}
