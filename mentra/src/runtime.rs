mod agent;
mod background;
mod builder;
mod control;
mod error;
mod handle;
mod intrinsic;
mod skill;
mod store;
mod task;
mod team;

use std::{collections::HashSet, path::Path};

use crate::{
    provider::{
        BuiltinProvider, ModelInfo, Provider, ProviderDescriptor, ProviderId, ProviderRegistry,
    },
    runtime::{builder::RuntimeBuilder, skill::SkillLoadError},
    tool::ExecutableTool,
};

pub(crate) use agent::AgentSpawnOptions;
pub use agent::{
    Agent, AgentConfig, AgentEvent, AgentSnapshot, AgentStatus, ContextCompactionConfig,
    ContextCompactionDetails, ContextCompactionTrigger, PendingAssistantTurn,
    PendingToolUseSummary, SpawnedAgentStatus, SpawnedAgentSummary, TaskConfig, TeamAutonomyConfig,
    TeamConfig, WorkspaceConfig,
};
pub use background::{BackgroundTaskStatus, BackgroundTaskSummary};
pub use control::{
    AuditHook, AuditLogHook, CancellationFlag, CancellationToken, CommandOutput, CommandRequest,
    CommandSpec, RunOptions, RuntimeExecutor, RuntimeHook, RuntimeHookEvent, RuntimeHooks,
    RuntimePolicy,
};
pub use error::RuntimeError;
pub(crate) use handle::RuntimeHandle;
pub(crate) use intrinsic::TASK_TOOL_NAME;
pub(crate) use store::{
    LoadedAgentState, PersistedAgentRecord, PersistedPendingTurn, TaskStateSnapshot,
};
pub use store::{RuntimeStore, SqliteRuntimeStore};
pub(crate) use task::{
    TASK_CREATE_TOOL_NAME, TASK_GET_TOOL_NAME, TASK_LIST_TOOL_NAME, TASK_UPDATE_TOOL_NAME,
};
pub use task::{TaskItem, TaskStatus};
pub use team::{
    TeamDispatch, TeamMemberStatus, TeamMemberSummary, TeamMessage, TeamProtocolRequestSummary,
    TeamProtocolStatus,
};

/// Entry point for configuring providers, tools, and agent lifecycles.
pub struct Runtime {
    handle: RuntimeHandle,
    provider_registry: ProviderRegistry,
}

impl Runtime {
    /// Returns a builder with Mentra's builtin tools enabled.
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::new()
    }

    /// Returns a builder with no builtin tools registered.
    pub fn empty_builder() -> RuntimeBuilder {
        RuntimeBuilder::new_empty()
    }

    /// Registers a custom tool on the runtime after construction.
    pub fn register_tool<T>(&self, tool: T)
    where
        T: ExecutableTool + 'static,
    {
        self.handle.register_tool(tool);
    }

    /// Registers a skills directory and enables the builtin `load_skill` tool.
    pub fn register_skills_dir(&self, path: impl AsRef<Path>) -> Result<(), SkillLoadError> {
        self.handle
            .register_skill_loader(skill::SkillLoader::from_dir(path)?);
        Ok(())
    }

    /// Spawns a new agent with the default [`AgentConfig`].
    pub fn spawn(&self, name: impl Into<String>, model: ModelInfo) -> Result<Agent, RuntimeError> {
        self.spawn_with_config(name, model, AgentConfig::default())
    }

    /// Spawns a new agent with an explicit configuration.
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
                .get_provider(Some(&model.provider))
                .ok_or_else(|| RuntimeError::ProviderNotFound(Some(model.provider.clone())))?,
            AgentSpawnOptions {
                hidden_tools: HashSet::new(),
                ..AgentSpawnOptions::default()
            },
        )
    }

    /// Restores a previously persisted agent by identifier.
    pub fn resume_agent(&self, agent_id: &str) -> Result<Agent, RuntimeError> {
        let Some(state) = self.handle.store().load_agent(agent_id)? else {
            return Err(RuntimeError::Store(format!(
                "No persisted agent with id '{agent_id}'"
            )));
        };
        let provider = self
            .provider_registry
            .get_provider(Some(&state.record.provider_id))
            .ok_or_else(|| {
                RuntimeError::ProviderNotFound(Some(state.record.provider_id.clone()))
            })?;
        Agent::from_loaded(self.handle.clone(), state, provider)
    }

    /// Restores every persisted agent known to the runtime store.
    pub fn resume_all(&self) -> Result<Vec<Agent>, RuntimeError> {
        let states = self.handle.store().list_agents()?;
        let mut agents = Vec::new();
        for state in states {
            let provider = self
                .provider_registry
                .get_provider(Some(&state.record.provider_id))
                .ok_or_else(|| {
                    RuntimeError::ProviderNotFound(Some(state.record.provider_id.clone()))
                })?;
            agents.push(Agent::from_loaded(self.handle.clone(), state, provider)?);
        }
        Ok(agents)
    }
}

impl Runtime {
    /// Returns descriptors for registered providers.
    pub fn providers(&self) -> Vec<ProviderDescriptor> {
        self.provider_registry.descriptors()
    }

    /// Registers a builtin provider from an API key.
    pub fn register_provider(
        &mut self,
        id: BuiltinProvider,
        api_key: impl Into<String>,
    ) -> Result<(), String> {
        self.provider_registry
            .register_builtin_provider(id, api_key)
    }

    /// Registers a custom provider implementation.
    pub fn register_provider_instance<P>(&mut self, provider: P)
    where
        P: Provider + 'static,
    {
        self.provider_registry.register_provider_instance(provider);
    }

    /// Lists models for a specific provider, or the default provider when omitted.
    pub async fn list_models(
        &self,
        provider: Option<&ProviderId>,
    ) -> Result<Vec<ModelInfo>, RuntimeError> {
        self.provider_registry
            .get_provider(provider)
            .ok_or_else(|| RuntimeError::ProviderNotFound(provider.cloned()))?
            .list_models()
            .await
            .map_err(RuntimeError::FailedToListModels)
    }
}
