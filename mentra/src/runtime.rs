mod builder;
pub(crate) mod control;
mod error;
pub(crate) mod handle;
mod intrinsic;
mod skill;
mod store;
pub(crate) mod task;

use std::path::Path;

use crate::{
    agent::{Agent, AgentConfig, AgentSpawnOptions, AgentStatus},
    provider::{
        BuiltinProvider, ModelInfo, Provider, ProviderDescriptor, ProviderId, ProviderRegistry,
    },
    runtime::{builder::RuntimeBuilder, skill::SkillLoadError},
    tool::ExecutableTool,
};

pub use crate::background::{BackgroundTaskStatus, BackgroundTaskSummary};
pub use crate::team::{
    TeamDispatch, TeamMemberStatus, TeamMemberSummary, TeamMessage, TeamMessageKind,
    TeamProtocolRequestSummary, TeamProtocolStatus,
};
pub use control::{
    AuditHook, AuditLogHook, CancellationFlag, CancellationToken, CommandOutput, CommandRequest,
    CommandSpec, RunOptions, RuntimeExecutor, RuntimeHook, RuntimeHookEvent, RuntimeHooks,
    RuntimePolicy,
};
pub use error::RuntimeError;
pub(crate) use handle::RuntimeHandle;
pub(crate) use intrinsic::RuntimeIntrinsicTool;
pub(crate) use store::{LoadedAgentState, PersistedAgentRecord, TaskStateSnapshot};
pub use store::{RuntimeStore, SqliteRuntimeStore};
pub(crate) use task::TaskIntrinsicTool;
pub use task::{TaskItem, TaskStatus};

/// Entry point for configuring providers, tools, and agent lifecycles.
pub struct Runtime {
    handle: RuntimeHandle,
    provider_registry: ProviderRegistry,
}

/// Read-only summary of a persisted agent record for a runtime identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedAgentSummary {
    pub id: String,
    pub runtime_identifier: String,
    pub name: String,
    pub is_teammate: bool,
    pub status: AgentStatus,
    pub history_len: usize,
}

impl Runtime {
    /// Returns a builder with Mentra's builtin tools enabled.
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::new(true)
    }

    /// Returns a builder with no builtin tools registered.
    pub fn empty_builder() -> RuntimeBuilder {
        RuntimeBuilder::new(false)
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
            AgentSpawnOptions::default(),
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

    /// Restores every persisted agent that belongs to the provided runtime identifier.
    pub fn resume(&self, runtime_identifier: &str) -> Result<Vec<Agent>, RuntimeError> {
        let states = self
            .handle
            .store()
            .list_agents_by_runtime(runtime_identifier)?;
        let mut agents = Vec::new();
        for state in states {
            let provider = self
                .provider_registry
                .get_provider(Some(&state.record.provider_id))
                .ok_or_else(|| {
                    RuntimeError::ProviderNotFound(Some(state.record.provider_id.clone()))
                })?;
            let agent = Agent::from_loaded(self.handle.clone(), state, provider)?;
            if agent.is_teammate() {
                agent.revive_teammate_actor()?;
            } else {
                agents.push(agent);
            }
        }
        Ok(agents)
    }

    /// Lists persisted agents for a runtime identifier without reviving them.
    pub fn list_persisted_agents(
        &self,
        runtime_identifier: &str,
    ) -> Result<Vec<PersistedAgentSummary>, RuntimeError> {
        self.handle
            .store()
            .list_agents_by_runtime(runtime_identifier)
            .map(|states| {
                states
                    .into_iter()
                    .map(|state| PersistedAgentSummary {
                        id: state.record.id,
                        runtime_identifier: state.record.runtime_identifier,
                        name: state.record.name,
                        is_teammate: state.record.teammate_identity.is_some(),
                        status: state.record.status,
                        history_len: state.memory.transcript.len(),
                    })
                    .collect()
            })
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
