#[path = "intrinsic/descriptor.rs"]
mod descriptor;
#[path = "intrinsic/execute.rs"]
mod execute;

use async_trait::async_trait;
use strum::{Display, VariantArray};

use crate::tool::{RuntimeToolDescriptor, ToolContext, ToolDefinition, ToolExecutor, ToolResult};

#[derive(Clone, Copy, Display, VariantArray)]
#[strum(prefix = "task_")]
#[strum(serialize_all = "snake_case")]
pub enum TaskIntrinsicTool {
    Create,
    Claim,
    Update,
    List,
    Get,
}

impl ToolDefinition for TaskIntrinsicTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        descriptor::task_intrinsic_descriptor(*self)
    }
}

#[async_trait]
impl ToolExecutor for TaskIntrinsicTool {
    async fn execute_mut(&self, ctx: ToolContext<'_>, input: serde_json::Value) -> ToolResult {
        execute::execute_mut(*self, ctx, input)
    }
}
