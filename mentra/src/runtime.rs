mod agent;
mod error;
mod handle;
mod task;
mod todo;

use std::{
    collections::HashSet,
    sync::{Arc, RwLock},
};

use crate::{
    provider::{
        Provider, ProviderRegistry,
        model::{ModelInfo, ModelProviderKind},
    },
    runtime::{error::RuntimeError, handle::RuntimeHandle},
    tool::{ToolHandler, ToolRegistry},
};

pub use agent::{
    Agent, AgentConfig, AgentEvent, AgentSnapshot, AgentStatus, PendingAssistantTurn,
    PendingToolUseSummary, SpawnedAgentStatus, SpawnedAgentSummary,
};
pub(crate) use task::TASK_TOOL_NAME;
pub(crate) use todo::TODO_TOOL_NAME;
pub use todo::{TodoItem, TodoStatus};

#[derive(Default)]
pub struct Runtime {
    tool_registry: Arc<RwLock<ToolRegistry>>,
    provider_registry: ProviderRegistry,
}

impl From<&Runtime> for RuntimeHandle {
    fn from(runtime: &Runtime) -> Self {
        Self {
            tool_registry: Arc::clone(&runtime.tool_registry),
        }
    }
}

impl Runtime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn new_empty() -> Self {
        Self {
            tool_registry: Arc::new(RwLock::new(ToolRegistry::new_empty())),
            provider_registry: ProviderRegistry::default(),
        }
    }

    pub fn register_tool<T>(&self, tool: T)
    where
        T: ToolHandler + 'static,
    {
        self.tool_registry
            .write()
            .expect("tool registry poisoned")
            .register_tool(tool);
    }

    pub fn spawn(&self, name: impl Into<String>, model: ModelInfo) -> Result<Agent, RuntimeError> {
        self.spawn_with_config(name, model, AgentConfig::default())
    }

    pub fn spawn_with_config(
        &self,
        name: impl Into<String>,
        model: ModelInfo,
        config: AgentConfig,
    ) -> Result<Agent, RuntimeError> {
        Ok(Agent::new(
            self.into(),
            model.id,
            name.into(),
            config,
            self.provider_registry
                .get_provider(Some(model.provider))
                .ok_or_else(|| RuntimeError::ProviderNotFound(Some(model.provider)))?,
            HashSet::new(),
            None,
        ))
    }
}

impl Runtime {
    pub fn providers(&self) -> Vec<ModelProviderKind> {
        self.provider_registry.providers()
    }

    pub fn register_provider(&mut self, kind: ModelProviderKind, api_key: impl Into<String>) {
        self.provider_registry.register_provider(kind, api_key);
    }

    pub fn register_provider_instance<P>(&mut self, provider: P)
    where
        P: Provider + 'static,
    {
        self.provider_registry.register_provider_instance(provider);
    }

    pub async fn list_models(
        &self,
        provider: Option<ModelProviderKind>,
    ) -> Result<Vec<ModelInfo>, RuntimeError> {
        self.provider_registry
            .get_provider(provider)
            .ok_or(RuntimeError::ProviderNotFound(provider))?
            .list_models()
            .await
            .map_err(RuntimeError::FailedToListModels)
    }
}
