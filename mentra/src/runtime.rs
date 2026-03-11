mod agent;
mod background;
mod builder;
mod error;
mod handle;
mod skill;
mod task;
mod task_graph;

use std::{collections::HashSet, path::Path};

use crate::{
    provider::{
        Provider, ProviderRegistry,
        model::{ModelInfo, ModelProviderKind},
    },
    runtime::{builder::RuntimeBuilder, error::RuntimeError, skill::SkillLoadError},
    tool::ToolHandler,
};

pub use agent::{
    Agent, AgentConfig, AgentEvent, AgentSnapshot, AgentStatus, ContextCompactionConfig,
    ContextCompactionDetails, ContextCompactionTrigger, PendingAssistantTurn,
    PendingToolUseSummary, SpawnedAgentStatus, SpawnedAgentSummary, TaskGraphConfig,
};
pub use background::{BackgroundTaskStatus, BackgroundTaskSummary};
pub(crate) use handle::RuntimeHandle;
pub(crate) const COMPACT_TOOL_NAME: &str = "compact";
pub(crate) use task::TASK_TOOL_NAME;
pub(crate) use task_graph::{
    TASK_CREATE_TOOL_NAME, TASK_GET_TOOL_NAME, TASK_LIST_TOOL_NAME, TASK_UPDATE_TOOL_NAME,
    TaskDiskState, TaskGraphError, TaskStore, is_task_graph_tool,
};
pub use task_graph::{TaskItem, TaskStatus};

pub struct Runtime {
    handle: RuntimeHandle,
    provider_registry: ProviderRegistry,
}

impl Runtime {
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::new()
    }

    pub fn empty_builder() -> RuntimeBuilder {
        RuntimeBuilder::new_empty()
    }

    pub fn register_tool<T>(&self, tool: T)
    where
        T: ToolHandler + 'static,
    {
        self.handle.register_tool(tool);
    }

    pub fn register_skills_dir(&self, path: impl AsRef<Path>) -> Result<(), SkillLoadError> {
        self.handle
            .register_skill_loader(skill::SkillLoader::from_dir(path)?);
        Ok(())
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
        Agent::new(
            self.handle.clone(),
            model.id,
            name.into(),
            config,
            self.provider_registry
                .get_provider(Some(model.provider))
                .ok_or_else(|| RuntimeError::ProviderNotFound(Some(model.provider)))?,
            HashSet::new(),
            None,
        )
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
