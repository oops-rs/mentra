mod builtin;
mod context;
mod files;
mod model;

use std::{collections::HashMap, sync::Arc};

pub use model::{
    ExecutableTool, ParallelToolContext, ToolCall, ToolCapability, ToolContext, ToolDurability,
    ToolExecutionMode, ToolResult, ToolSideEffectLevel, ToolSpec,
};

use builtin::{BackgroundRunTool, CheckBackgroundTool, LoadSkillTool, ShellTool};
use files::FilesTool;

#[derive(Clone)]
struct RegisteredTool {
    spec: ToolSpec,
    handler: Arc<dyn ExecutableTool>,
}

#[derive(Clone, Default)]
/// Registry of tools available to a runtime instance.
pub struct ToolRegistry {
    tools: HashMap<String, RegisteredTool>,
    tool_specs: Arc<[ToolSpec]>,
}

impl ToolRegistry {
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

impl ToolRegistry {
    pub(crate) fn register_skill_tool(&mut self) {
        self.register_tool(LoadSkillTool);
    }

    pub(crate) fn register_builtin_tools(&mut self) {
        self.register_tool(ShellTool);
        self.register_tool(BackgroundRunTool);
        self.register_tool(CheckBackgroundTool);
        self.register_tool(FilesTool);
    }
}
