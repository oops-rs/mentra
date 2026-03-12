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
        T: ExecutableTool + 'static,
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
                .get_provider(Some(&model.provider))
                .ok_or_else(|| RuntimeError::ProviderNotFound(Some(model.provider.clone())))?,
            AgentSpawnOptions {
                hidden_tools: HashSet::new(),
                ..AgentSpawnOptions::default()
            },
        )
    }

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
    pub fn providers(&self) -> Vec<ProviderDescriptor> {
        self.provider_registry.descriptors()
    }

    pub fn register_provider(
        &mut self,
        id: BuiltinProvider,
        api_key: impl Into<String>,
    ) -> Result<(), String> {
        self.provider_registry
            .register_builtin_provider(id, api_key)
    }

    pub fn register_provider_instance<P>(&mut self, provider: P)
    where
        P: Provider + 'static,
    {
        self.provider_registry.register_provider_instance(provider);
    }

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
