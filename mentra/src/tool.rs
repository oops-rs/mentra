pub mod builtin;
mod model;

use std::{collections::HashMap, sync::Arc};

pub use model::{
    ExecutableTool, ToolCall, ToolCapability, ToolContext, ToolDurability, ToolResult,
    ToolSideEffectLevel, ToolSpec,
};

#[derive(Clone)]
struct RegisteredTool {
    spec: ToolSpec,
    handler: Arc<dyn ExecutableTool>,
}

#[derive(Clone)]
/// Registry of tools available to a runtime instance.
pub struct ToolRegistry {
    tools: HashMap<String, RegisteredTool>,
    tool_specs: Arc<[ToolSpec]>,
}

impl ToolRegistry {
    /// Creates an empty tool registry.
    pub fn new_empty() -> Self {
        Self {
            tools: HashMap::new(),
            tool_specs: Arc::from([]),
        }
    }

    /// Registers a tool implementation and refreshes the cached tool specs.
    pub fn register_tool<T>(&mut self, tool: T)
    where
        T: ExecutableTool + 'static,
    {
        let handler: Arc<dyn ExecutableTool> = Arc::new(tool);
        let spec = handler.spec();
        self.tools
            .insert(spec.name.clone(), RegisteredTool { spec, handler });
        self.refresh_tool_specs();
    }

    /// Returns the provider-facing tool specifications.
    pub fn tools(&self) -> Arc<[ToolSpec]> {
        Arc::clone(&self.tool_specs)
    }

    /// Returns a tool handler by name.
    pub fn get_tool(&self, name: &str) -> Option<Arc<dyn ExecutableTool>> {
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
