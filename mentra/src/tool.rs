mod authorization;
mod builtin;
mod context;
mod descriptor;
mod files;
pub(crate) mod internal;
mod model;
mod orchestrator;
mod runtime;

use std::{collections::HashMap, sync::Arc};

pub use authorization::{
    ToolAuthorizationDecision, ToolAuthorizationOutcome, ToolAuthorizationPreview,
    ToolAuthorizationRequest, ToolAuthorizer,
};
pub use descriptor::{
    ProviderToolSpec, RuntimeToolDescriptor, RuntimeToolDescriptorBuilder, ToolApprovalCategory,
    ToolCapability, ToolDurability, ToolExecutionCategory, ToolExecutionMode, ToolLoadingPolicy,
    ToolSideEffectLevel,
};
pub use model::{
    ExecutableTool, ParallelToolContext, ToolCall, ToolContext, ToolDefinition, ToolExecutor,
    ToolResult, ToolSpec,
};
pub(crate) use runtime::ToolRuntime;

use builtin::{BackgroundRunTool, CheckBackgroundTool, LoadSkillTool, ShellTool};
use files::FilesTool;

#[derive(Clone)]
struct RegisteredTool {
    descriptor: RuntimeToolDescriptor,
    handler: Arc<dyn ExecutableTool>,
}

#[derive(Clone, Default)]
/// Registry of tools available to a runtime instance.
pub struct ToolRegistry {
    tools: HashMap<String, RegisteredTool>,
    provider_specs: Arc<[ProviderToolSpec]>,
}

impl ToolRegistry {
    /// Registers a tool implementation and refreshes the cached tool specs.
    pub fn register_tool<T>(&mut self, tool: T)
    where
        T: ExecutableTool + 'static,
    {
        let handler: Arc<dyn ExecutableTool> = Arc::new(tool);
        let descriptor = handler.descriptor();
        self.tools.insert(
            descriptor.provider.name.clone(),
            RegisteredTool {
                descriptor,
                handler,
            },
        );
        self.refresh_provider_specs();
    }

    /// Returns the provider-facing tool specifications.
    pub fn tools(&self) -> Arc<[ProviderToolSpec]> {
        Arc::clone(&self.provider_specs)
    }

    /// Returns a tool handler by name.
    pub fn get_tool(&self, name: &str) -> Option<Arc<dyn ExecutableTool>> {
        self.tools.get(name).map(|tool| Arc::clone(&tool.handler))
    }

    pub fn get_tool_descriptor(&self, name: &str) -> Option<RuntimeToolDescriptor> {
        self.tools.get(name).map(|tool| tool.descriptor.clone())
    }

    fn refresh_provider_specs(&mut self) {
        self.provider_specs = self
            .tools
            .values()
            .map(|tool| tool.descriptor.provider.clone())
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
