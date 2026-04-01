mod builder;
pub(crate) mod control;
mod error;
pub(crate) mod handle;
mod hybrid_store;
mod intrinsic;
mod skill;
mod store;
pub(crate) mod task;

use std::{any::Any, path::Path, sync::Arc};

use crate::{
    agent::{Agent, AgentConfig, AgentSpawnOptions, AgentStatus},
    provider::{Provider, ProviderRegistry},
    runtime::{builder::RuntimeBuilder, skill::SkillLoadError},
    session::{Session, SessionEvent, SessionId, SessionMetadata},
    tool::ExecutableTool,
};
use mentra_provider::{BuiltinProvider, ModelInfo, ModelSelector, ProviderDescriptor, ProviderId};

pub use control::{
    AuditHook, AuditLogHook, CancellationFlag, CancellationToken, CommandOutput, CommandRequest,
    CommandSpec, ExecOutput, RunOptions, RuntimeExecutor, RuntimeHook, RuntimeHookEvent,
    RuntimeHooks, RuntimePolicy, is_transient_provider_error, is_transient_runtime_error,
};
pub use error::RuntimeError;
pub(crate) use handle::RuntimeHandle;
pub use hybrid_store::HybridRuntimeStore;
pub(crate) use intrinsic::RuntimeIntrinsicTool;
pub use store::{
    AgentStore, AuditStore, LeaseStore, PermissionRuleStore, RunStore, RuntimeStore,
    SqliteRuntimeStore, TaskStore,
};
pub(crate) use store::{LoadedAgentState, PersistedAgentRecord, TaskStateSnapshot};
pub(crate) use task::TaskIntrinsicTool;
pub use task::{TaskItem, TaskStatus};

/// Entry point for configuring providers, tools, and agent lifecycles.
///
/// A runtime composes four main subsystems:
/// - execution: providers, policies, hooks, and command execution
/// - persistence: agent state, runs, tasks, leases, and memory
/// - tooling: registered tools, skills, and app context
/// - collaboration: persistent teams and background task coordination
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

    /// Registers typed application state that tools can retrieve from their context.
    pub fn register_context(&self, context: Arc<dyn Any + Send + Sync>) {
        self.handle.register_app_context(context);
    }

    /// Returns typed application state previously registered on this runtime.
    pub fn app_context<T>(&self) -> Result<Arc<T>, String>
    where
        T: Any + Send + Sync + 'static,
    {
        self.handle.app_context::<T>()
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

    /// Registers the local Ollama provider using its default OpenAI-compatible endpoint.
    pub fn register_ollama(&mut self) {
        self.provider_registry.register_ollama();
    }

    /// Registers the local LM Studio provider using its default OpenAI-compatible endpoint.
    pub fn register_lmstudio(&mut self) {
        self.provider_registry.register_lmstudio();
    }

    /// Registers a custom provider implementation.
    ///
    /// This is the supported seam for injecting a scripted provider in tests or
    /// embedding Mentra on top of a custom transport.
    ///
    /// ```rust,no_run
    /// use async_trait::async_trait;
    /// use mentra::{BuiltinProvider, ModelInfo, ProviderDescriptor, Runtime};
    /// use mentra::error::{ProviderError, RuntimeError};
    /// use mentra::provider::{Provider, ProviderEventStream, Request};
    /// use tokio::sync::mpsc;
    ///
    /// struct TestProvider;
    ///
    /// #[async_trait]
    /// impl Provider for TestProvider {
    ///     fn descriptor(&self) -> ProviderDescriptor {
    ///         ProviderDescriptor::new(BuiltinProvider::Anthropic)
    ///     }
    ///
    ///     async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
    ///         Ok(vec![ModelInfo::new("test-model", BuiltinProvider::Anthropic)])
    ///     }
    ///
    ///     async fn stream(
    ///         &self,
    ///         _request: Request<'_>,
    ///     ) -> Result<ProviderEventStream, ProviderError> {
    ///         let (_tx, rx) = mpsc::unbounded_channel();
    ///         Ok(rx)
    ///     }
    /// }
    ///
    /// let mut runtime = Runtime::empty_builder()
    ///     .with_provider(BuiltinProvider::Anthropic, "placeholder")
    ///     .build()?;
    /// runtime.register_provider_instance(TestProvider);
    /// # Ok::<(), RuntimeError>(())
    /// ```
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

    /// Resolves a model for a registered provider using a deterministic selection strategy.
    pub async fn resolve_model(
        &self,
        provider: impl Into<ProviderId>,
        selector: ModelSelector,
    ) -> Result<ModelInfo, RuntimeError> {
        let provider = provider.into();
        if self
            .provider_registry
            .get_provider(Some(&provider))
            .is_none()
        {
            return Err(RuntimeError::ProviderNotFound(Some(provider)));
        }

        match selector {
            ModelSelector::Id(id) => Ok(ModelInfo::new(id, provider)),
            ModelSelector::NewestAvailable => {
                let mut models = self.list_models(Some(&provider)).await?;
                models.sort_by(|left, right| {
                    right
                        .created_at
                        .cmp(&left.created_at)
                        .then_with(|| left.id.cmp(&right.id))
                });
                models
                    .into_iter()
                    .next()
                    .ok_or(RuntimeError::NoModelsAvailable(provider))
            }
        }
    }
}

// -- Session lifecycle methods --

impl Runtime {
    /// Creates a new session wrapping a freshly spawned agent with default config.
    pub fn create_session(
        &self,
        name: impl Into<String>,
        model: ModelInfo,
    ) -> Result<Session, RuntimeError> {
        self.create_session_with_config(name, model, AgentConfig::default())
    }

    /// Creates a new session wrapping a freshly spawned agent with explicit config.
    pub fn create_session_with_config(
        &self,
        name: impl Into<String>,
        model: ModelInfo,
        config: AgentConfig,
    ) -> Result<Session, RuntimeError> {
        let name = name.into();
        let agent = self.spawn_with_config(&name, model.clone(), config)?;
        let session_id = SessionId::new();
        let metadata = SessionMetadata::new(session_id.clone(), &name, &model.id);
        let mut session = Session::new(session_id.clone(), metadata, agent);

        // Emit the initial SessionStarted event.
        let started = SessionEvent::SessionStarted { session_id };
        // Subscribe briefly just to ensure the event is broadcast.
        let _rx = session.subscribe();
        // Use the internal emit path via a helper on Session.
        session.emit_started(started);

        Ok(session)
    }

    /// Resumes a previously persisted agent and wraps it in a session.
    pub fn resume_session(&self, agent_id: &str) -> Result<Session, RuntimeError> {
        let agent = self.resume_agent(agent_id)?;
        let session_id = SessionId::new();
        let metadata = SessionMetadata::new(session_id.clone(), agent.name(), agent.model());
        let session = Session::new(session_id, metadata, agent);
        Ok(session)
    }
}
