mod agent;
mod error;
mod handle;
mod skill;
mod task;
mod todo;

use std::{collections::HashSet, path::Path};

use crate::{
    provider::{
        Provider, ProviderRegistry,
        model::{ModelInfo, ModelProviderKind},
    },
    runtime::error::RuntimeError,
    runtime::skill::SkillLoadError,
    tool::ToolHandler,
};

pub use agent::{
    Agent, AgentConfig, AgentEvent, AgentSnapshot, AgentStatus, PendingAssistantTurn,
    PendingToolUseSummary, SpawnedAgentStatus, SpawnedAgentSummary,
};
pub(crate) use handle::RuntimeHandle;
pub(crate) use task::TASK_TOOL_NAME;
pub(crate) use todo::TODO_TOOL_NAME;
pub use todo::{TodoItem, TodoStatus};

#[derive(Default)]
pub struct Runtime {
    handle: RuntimeHandle,
    provider_registry: ProviderRegistry,
}

impl Runtime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn new_empty() -> Self {
        Self {
            handle: RuntimeHandle::new_empty(),
            provider_registry: ProviderRegistry::default(),
        }
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
        Ok(Agent::new(
            self.handle.clone(),
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
