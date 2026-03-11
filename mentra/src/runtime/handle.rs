use std::sync::{Arc, RwLock};

use crate::{
    provider::model::ContentBlock,
    tool::{ToolCall, ToolContext, ToolRegistry, ToolSpec},
};

#[derive(Clone)]
pub struct RuntimeHandle {
    pub(crate) tool_registry: Arc<RwLock<ToolRegistry>>,
}

impl RuntimeHandle {
    pub fn tools(&self) -> Arc<[ToolSpec]> {
        self.tool_registry
            .read()
            .expect("tool registry poisoned")
            .tools()
    }

    pub async fn execute_tool(&self, tool_call: ToolCall) -> ContentBlock {
        let tool = self
            .tool_registry
            .read()
            .expect("tool registry poisoned")
            .get_tool(&tool_call.name);

        if let Some(tool) = tool {
            match tool
                .invoke(
                    ToolContext {
                        tool_call_id: tool_call.id.clone(),
                        tool_name: tool_call.name.clone(),
                    },
                    tool_call.input,
                )
                .await
            {
                Ok(content) => ContentBlock::ToolResult {
                    tool_use_id: tool_call.id,
                    content,
                    is_error: false,
                },
                Err(content) => ContentBlock::ToolResult {
                    tool_use_id: tool_call.id,
                    content,
                    is_error: true,
                },
            }
        } else {
            ContentBlock::ToolResult {
                tool_use_id: tool_call.id,
                content: "Tool not found".to_string(),
                is_error: true,
            }
        }
    }
}
