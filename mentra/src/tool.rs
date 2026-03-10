pub mod builtin;
mod model;

use std::{collections::HashMap, sync::Arc};

pub use model::{ToolCall, ToolContext, ToolHandler, ToolResult, ToolSpec};

struct RegisteredTool {
    spec: ToolSpec,
    handler: Arc<dyn ToolHandler>,
}

pub struct ToolRegistry {
    tools: HashMap<String, RegisteredTool>,
}

impl ToolRegistry {
    pub fn new_empty() -> Self {
        let mut registry = Self {
            tools: HashMap::new(),
        };
        registry.register_tool(builtin::TodoTool);
        registry
    }

    pub fn register_tool<T>(&mut self, tool: T)
    where
        T: ToolHandler + 'static,
    {
        let handler: Arc<dyn ToolHandler> = Arc::new(tool);
        let spec = handler.spec();
        self.tools
            .insert(spec.name.clone(), RegisteredTool { spec, handler });
    }

    pub fn tools(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|tool| tool.spec.clone()).collect()
    }

    pub fn get_tool(&self, name: &str) -> Option<Arc<dyn ToolHandler>> {
        self.tools.get(name).map(|tool| Arc::clone(&tool.handler))
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        let mut registry = Self::new_empty();
        registry.register_tool(builtin::BashTool);
        registry.register_tool(builtin::ReadFileTool);
        registry
    }
}
