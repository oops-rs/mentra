mod execute;
mod schema;

use async_trait::async_trait;
use strum::{Display, VariantArray};

use crate::{
    ContentBlock,
    tool::{
        ParallelToolContext, RuntimeToolDescriptor, ToolCall, ToolContext, ToolDefinition,
        ToolExecutor, ToolResult,
    },
};

#[derive(Clone, Copy, Debug, Display, VariantArray)]
#[strum(prefix = "team_")]
#[strum(serialize_all = "snake_case")]
pub(crate) enum TeamIntrinsicTool {
    Spawn,
    Send,
    ReadInbox,
    Broadcast,
    Request,
    Respond,
    ListRequests,
}

impl ToolDefinition for TeamIntrinsicTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        self.tool_spec()
    }
}

#[async_trait]
impl ToolExecutor for TeamIntrinsicTool {
    async fn execute(&self, ctx: ParallelToolContext, input: serde_json::Value) -> ToolResult {
        match self {
            Self::ListRequests => execute::execute_team_list_requests_parallel(ctx, input),
            _ => Err(format!(
                "Tool '{}' does not support parallel execution",
                self.descriptor().provider.name
            )),
        }
    }

    async fn execute_mut(&self, ctx: ToolContext<'_>, input: serde_json::Value) -> ToolResult {
        match self {
            Self::ListRequests => execute::execute_team_list_requests_parallel(ctx.into(), input),
            _ => {
                let call = ToolCall {
                    id: ctx.tool_call_id.clone(),
                    name: self.to_string(),
                    input,
                };
                let block = match self {
                    Self::Spawn => execute::execute_team_spawn(ctx.agent, call).await,
                    Self::Send => execute::execute_team_send(ctx.agent, call),
                    Self::ReadInbox => execute::execute_team_read_inbox(ctx.agent, call),
                    Self::Broadcast => execute::execute_team_broadcast(ctx.agent, call),
                    Self::Request => execute::execute_team_request(ctx.agent, call),
                    Self::Respond => execute::execute_team_respond(ctx.agent, call),
                    Self::ListRequests => unreachable!("handled above"),
                };
                content_block_to_result(block)
            }
        }
    }
}

fn content_block_to_result(block: ContentBlock) -> ToolResult {
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
        _ => Err("Team intrinsic returned an unexpected content block".to_string()),
    }
}
