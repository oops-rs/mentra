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
    tool_specs: Arc<[ToolSpec]>,
}

impl ToolRegistry {
    pub fn new_empty() -> Self {
        Self {
            tools: HashMap::new(),
            tool_specs: Arc::from([]),
        }
    }

    pub fn register_tool<T>(&mut self, tool: T)
    where
        T: ToolHandler + 'static,
    {
        let handler: Arc<dyn ToolHandler> = Arc::new(tool);
        let spec = handler.spec();
        self.tools
            .insert(spec.name.clone(), RegisteredTool { spec, handler });
        self.refresh_tool_specs();
    }

    pub fn tools(&self) -> Arc<[ToolSpec]> {
        Arc::clone(&self.tool_specs)
    }

    pub fn get_tool(&self, name: &str) -> Option<Arc<dyn ToolHandler>> {
        self.tools.get(name).map(|tool| Arc::clone(&tool.handler))
    }

    fn refresh_tool_specs(&mut self) {
        self.tool_specs = self
            .tools
            .values()
            .map(|tool| tool.spec.clone())
            .collect::<Vec<_>>()
            .into();
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        let mut registry = Self::new_empty();
        registry.register_tool(builtin::BashTool);
        registry.register_tool(builtin::BackgroundRunTool);
        registry.register_tool(builtin::CheckBackgroundTool);
        registry.register_tool(builtin::ReadFileTool);
        registry
    }
}
