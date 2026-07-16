mod authorization;
/// Bash command validation — safety checks before shell execution.
pub mod bash_validation;
mod builtin;
mod context;
mod descriptor;
mod files;
pub(crate) mod internal;
mod model;
mod orchestrator;
mod runtime;
mod truncation;

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
pub use mentra_provider::ToolResultContent;
pub use model::{
    ExecutableTool, ParallelToolContext, ToolCall, ToolContext, ToolDefinition, ToolExecutor,
    ToolOutput, ToolResult, ToolSpec,
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

    pub(crate) fn unregister_tool(&mut self, name: &str) -> bool {
        let removed = self.tools.remove(name).is_some();
        if removed {
            self.refresh_provider_specs();
        }
        removed
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

#[cfg(test)]
mod tests {
    use std::{borrow::Cow, collections::BTreeMap};

    use serde_json::json;

    use super::*;

    #[test]
    fn builtin_shell_and_files_tools_serialize_as_non_strict_responses_functions() {
        let mut registry = ToolRegistry::default();
        registry.register_builtin_tools();

        let request = mentra_provider::Request {
            model: Cow::Borrowed("gpt-5"),
            system: None,
            messages: Cow::Owned(Vec::new()),
            tools: Cow::Owned(registry.tools().to_vec()),
            tool_choice: None,
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
            provider_request_options: mentra_provider::ProviderRequestOptions::default(),
        };

        let payload = serde_json::to_value(
            mentra_provider::responses::model::ResponsesRequest::try_from(request)
                .expect("built-in tools should serialize for Responses"),
        )
        .expect("responses request should serialize");
        let tools = payload["tools"]
            .as_array()
            .expect("tools should be a json array");

        for name in ["shell", "background_run", "files"] {
            let tool = tools
                .iter()
                .find(|tool| tool["name"] == json!(name))
                .unwrap_or_else(|| panic!("{name} tool should be serialized"));
            assert_eq!(tool["type"], "function");
            assert_eq!(tool["strict"], false);
        }
    }
}
