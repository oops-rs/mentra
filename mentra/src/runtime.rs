mod builder;
pub(crate) mod control;
mod error;
mod file_store;
pub(crate) mod handle;
#[cfg(feature = "store-sqlite")]
mod hybrid_store;
mod intrinsic;
mod skill;
#[cfg(feature = "store-sqlite")]
mod sqlite_store;
mod store;
pub(crate) mod task;
mod task_board;
mod volatile_store;

use std::{any::Any, path::Path, sync::Arc};

use tokio::sync::broadcast;

use crate::{
    agent::{Agent, AgentConfig, AgentSpawnOptions, AgentStatus},
    provider::{Provider, ProviderRegistry, ProviderSessionScope},
    session::{
        Session, SessionEvent, SessionId, SessionMetadata, hooks::SessionHookBridge,
        permission::PendingPermissionStore,
    },
    tool::ExecutableTool,
};
use mentra_provider::{BuiltinProvider, ModelInfo, ModelSelector, ProviderDescriptor, ProviderId};

pub use builder::RuntimeBuilder;
pub use control::sandbox::{ExecutionEnvironment, detect_environment};
pub use control::{
    AfterDecision, AuditHook, AuditLogHook, BeforeDecision, CancellationFlag, CancellationToken,
    CommandOutput, CommandRequest, CommandSpec, EarlyEnd, ExecOutput, ExecutionHookParticipant,
    ExecutionHookRegistration, ExecutionHookSnapshot, ExecutionHooks, HookDecision,
    LocalRuntimeExecutor, PostExecutionContext, PostExecutionHook, PostExecutionHookRegistration,
    PostExecutionHooks, PreExecutionContext, PreExecutionHook, PreExecutionHookRegistration,
    PreExecutionHooks, ProviderRetry, ResultDecision, RunOptions, RuntimeExecutor, RuntimeHook,
    RuntimeHookEvent, RuntimeHooks, RuntimePolicy, ShellValidationMode,
    is_transient_provider_error, is_transient_runtime_error, normalize_policy_root,
};
pub use error::{ErrorCategory, RuntimeError};
pub use file_store::FileRuntimeStore;
pub(crate) use handle::RuntimeHandle;
#[cfg(feature = "store-sqlite")]
pub use hybrid_store::HybridRuntimeStore;
pub(crate) use intrinsic::RuntimeIntrinsicTool;
pub use skill::{SkillInfo, SkillLoadError, skill_root_key};
#[cfg(feature = "store-sqlite")]
pub use sqlite_store::SqliteRuntimeStore;
pub use store::{
    AgentStore, AuditStore, LeaseStore, PermissionRuleContext, PermissionRuleStore, RunStore,
    RuntimeStore, TaskStore,
};
pub(crate) use store::{LoadedAgentState, PersistedAgentRecord, TaskStateSnapshot};
pub(crate) use task::TaskIntrinsicTool;
pub use task::{TaskItem, TaskStatus};
pub use task_board::{NewTask, TaskBoard, TaskBoardError, TaskPatch};
pub use volatile_store::VolatileRuntimeStore;

/// Entry point for configuring providers, tools, and agent lifecycles.
///
/// A runtime composes four main subsystems:
/// - execution: providers, policies, hooks, and command execution
/// - persistence: agent state, runs, tasks, leases, and memory
/// - tooling: registered tools, skills, and app context
/// - collaboration: persistent teams and background task coordination
pub struct Runtime {
    handle: RuntimeHandle,
    provider_registry: Arc<std::sync::RwLock<ProviderRegistry>>,
    pub(crate) mcp_servers: Vec<McpServerSummary>,
}

/// How one configured MCP server fared during
/// [`build_async`](RuntimeBuilder::build_async).
///
/// A server that fails to connect leaves the runtime in degraded mode rather
/// than failing the build — one unreachable server should not sink a session.
/// This is how a host finds out which ones are actually live, so it can say so
/// instead of leaving a user to wonder why a tool is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerSummary {
    pub name: String,
    /// Tools this server contributed. Zero when it failed.
    pub tools: usize,
    /// Why it did not connect, when it did not.
    pub error: Option<String>,
}

impl McpServerSummary {
    pub fn connected(&self) -> bool {
        self.error.is_none()
    }
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
    /// When the store first wrote this agent, in seconds since the epoch.
    ///
    /// `None` from a store that keeps nothing across process lifetimes. A host
    /// listing sessions needs these to order them by recency, which was
    /// otherwise impossible even though the `agents` table has carried both
    /// columns all along.
    pub created_at: Option<u64>,
    /// When the store last wrote this agent, in seconds since the epoch.
    pub updated_at: Option<u64>,
}

/// How a session is configured, scoped, and tagged in the store.
#[derive(Debug, Clone, Default)]
pub struct SessionOptions {
    pub config: AgentConfig,
    /// Complete runtime policy for this live session and its descendants.
    ///
    /// `None` inherits the policy installed on [`RuntimeBuilder`]. `Some`
    /// replaces that policy wholesale for this session; the two policies are
    /// not merged or intersected. The attachment is live-only and is not
    /// persisted in [`AgentConfig`], so a resumed session must receive its
    /// current policy again through [`SessionResumeOptions`].
    ///
    /// Like every [`RuntimePolicy`], this governs Mentra's builtin file and
    /// command paths but is not an OS filesystem or network sandbox.
    pub policy: Option<RuntimePolicy>,
    /// Ephemeral tool audience for this live session and its descendants.
    pub tool_audience: Option<crate::tool::ToolAudience>,
    /// Scopes this session's permission rules to a project.
    pub project_id: Option<String>,
    /// The runtime identifier this session's persisted rows are tagged with.
    ///
    /// `None` uses the runtime's own, which is what every session got before:
    /// one tag for every session on a runtime, and a
    /// [`list_persisted_agents`](Runtime::list_persisted_agents) that cannot
    /// separate one workspace's sessions from another's.
    pub runtime_identifier: Option<std::sync::Arc<str>>,
}

/// Ephemeral scope applied while resuming a persisted agent as a session.
#[derive(Debug, Clone, Default)]
pub struct SessionResumeOptions {
    /// Project scope used by resumed session permission rules.
    pub project_id: Option<String>,
    /// Complete runtime policy for this live resume and its descendants.
    ///
    /// `None` inherits the current runtime's policy. `Some` replaces it
    /// wholesale for this live session; it does not merge with the runtime
    /// policy or restore a policy from the persisted agent. See
    /// [`SessionOptions::policy`] for the confinement boundary.
    pub policy: Option<RuntimePolicy>,
    /// Ephemeral tool audience for this live resume and its descendants.
    pub tool_audience: Option<crate::tool::ToolAudience>,
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

    /// Returns a skill's body, whether or not the model may invoke it.
    ///
    /// The path a host uses to run a skill itself — as a slash command, say.
    /// `load_skill` refuses a skill whose frontmatter set
    /// `disable-model-invocation`, and that refusal is the point; without this
    /// such a skill appeared in [`skills`](Self::skills) and could be run by
    /// nobody, which made the flag's promise false.
    pub fn skill_body(&self, name: &str) -> Result<String, String> {
        self.handle.skill_body(name)
    }

    /// Registers a custom tool on the runtime after construction.
    pub fn register_tool<T>(&self, tool: T)
    where
        T: ExecutableTool + 'static,
    {
        self.handle.register_tool(tool);
    }

    /// Registers a custom tool unless its name is already taken.
    ///
    /// [`register_tool`](Self::register_tool) replaces a tool of the same
    /// name, which is right for deliberately overriding a builtin and wrong
    /// for a loader that did not mean to shadow one. This reports the
    /// collision instead.
    pub fn try_register_tool<T>(&self, tool: T) -> Result<(), crate::tool::ToolNameCollision>
    where
        T: ExecutableTool + 'static,
    {
        self.handle.try_register_tool(tool)
    }

    /// Registers a custom tool visible only to agents in `audience`.
    ///
    /// Registration refuses a global tool of the same name or another tool in
    /// the same audience, while a different audience may use the same name.
    /// The returned guard owns the registration lifetime and exposes the exact
    /// descriptor snapshot evaluated by this call.
    pub fn try_register_tool_for_audience<T>(
        &self,
        audience: crate::tool::ToolAudience,
        tool: T,
    ) -> Result<crate::tool::AudienceToolRegistration, crate::tool::ToolNameCollision>
    where
        T: ExecutableTool + 'static,
    {
        self.handle.try_register_tool_for_audience(audience, tool)
    }

    /// Removes a registered tool by name, reporting whether one was there.
    pub fn unregister_tool(&self, name: &str) -> bool {
        self.handle.unregister_tool_by_name(name)
    }

    /// Returns descriptors for registered tools in a deterministic order.
    pub fn tools(&self) -> Vec<crate::tool::RuntimeToolDescriptor> {
        self.tools_for_audience(None)
    }

    /// Returns the name-ordered tool descriptors visible to `audience`.
    ///
    /// `Some(audience)` resolves the matching audience namespace before the
    /// runtime-global fallback. `None` returns only runtime-global tools and is
    /// identical to [`tools`](Self::tools).
    ///
    /// No agent identity is supplied, so exact-agent registrations are
    /// intentionally excluded. The returned descriptor snapshot is cloned
    /// under one registry read lock and grants no authority to execute a tool.
    pub fn tools_for_audience(
        &self,
        audience: Option<&crate::tool::ToolAudience>,
    ) -> Vec<crate::tool::RuntimeToolDescriptor> {
        self.handle.visible_tool_descriptors_for_audience(audience)
    }

    /// Returns the descriptor for a registered tool by name.
    pub fn tool_descriptor(&self, name: &str) -> Option<crate::tool::RuntimeToolDescriptor> {
        self.handle.get_tool_descriptor(name)
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

    /// Registers a live pre-execution hook for every agent on this runtime.
    ///
    /// Agents and sessions that already exist observe the hook on their next
    /// tool call. Builder-time hooks are permanent and run first; live global
    /// and matching-audience hooks then run together in registration order.
    /// Keep the returned guard alive for as long as the hook should apply.
    pub fn register_pre_hook<H>(&self, hook: H) -> PreExecutionHookRegistration
    where
        H: PreExecutionHook + 'static,
    {
        self.handle.pre_hooks().register_live(None, hook)
    }

    /// Registers a live pre-execution hook for one [`ToolAudience`].
    ///
    /// The audience is opaque execution scope, not a working directory. Agents
    /// with another audience, and agents with no audience, never run this hook.
    ///
    /// [`ToolAudience`]: crate::tool::ToolAudience
    pub fn register_pre_hook_for_audience<H>(
        &self,
        audience: crate::tool::ToolAudience,
        hook: H,
    ) -> PreExecutionHookRegistration
    where
        H: PreExecutionHook + 'static,
    {
        self.handle.pre_hooks().register_live(Some(audience), hook)
    }

    /// Registers a live post-execution hook for every agent on this runtime.
    ///
    /// Builder-time hooks are permanent and outermost. Live hooks join one
    /// registration order with them, then the complete post-execution chain
    /// runs in exact reverse so the earliest registration has the final say.
    /// A post-hook invocation already snapshotted may finish after its guard is
    /// dropped; this does not retain the hook across the whole tool call.
    pub fn register_post_hook<H>(&self, hook: H) -> PostExecutionHookRegistration
    where
        H: PostExecutionHook + 'static,
    {
        self.handle.post_hooks().register_live(None, hook)
    }

    /// Registers a live post-execution hook for one [`ToolAudience`].
    ///
    /// The audience is opaque execution scope, not a working directory. Agents
    /// with another audience, and agents with no audience, never run this hook.
    ///
    /// [`ToolAudience`]: crate::tool::ToolAudience
    pub fn register_post_hook_for_audience<H>(
        &self,
        audience: crate::tool::ToolAudience,
        hook: H,
    ) -> PostExecutionHookRegistration
    where
        H: PostExecutionHook + 'static,
    {
        self.handle.post_hooks().register_live(Some(audience), hook)
    }

    /// Registers one live runtime-global participant in the ordered mixed chain.
    pub fn register_execution_hook<H>(&self, participant: H) -> ExecutionHookRegistration
    where
        H: ExecutionHookParticipant + 'static,
    {
        self.register_execution_hooks([Arc::new(participant) as Arc<dyn ExecutionHookParticipant>])
    }

    /// Atomically registers one ordered runtime-global participant batch.
    pub fn register_execution_hooks<I>(&self, participants: I) -> ExecutionHookRegistration
    where
        I: IntoIterator<Item = Arc<dyn ExecutionHookParticipant>>,
    {
        self.handle
            .execution_hooks()
            .register_live(None, participants.into_iter().collect())
    }

    /// Registers one live participant for an exact [`crate::tool::ToolAudience`].
    pub fn register_execution_hook_for_audience<H>(
        &self,
        audience: crate::tool::ToolAudience,
        participant: H,
    ) -> ExecutionHookRegistration
    where
        H: ExecutionHookParticipant + 'static,
    {
        self.register_execution_hooks_for_audience(
            audience,
            [Arc::new(participant) as Arc<dyn ExecutionHookParticipant>],
        )
    }

    /// Atomically registers one ordered batch for an exact [`ToolAudience`].
    ///
    /// Existing matching agents observe the complete batch on their next
    /// admitted call. The returned guard removes the batch as one unit.
    ///
    /// [`ToolAudience`]: crate::tool::ToolAudience
    pub fn register_execution_hooks_for_audience<I>(
        &self,
        audience: crate::tool::ToolAudience,
        participants: I,
    ) -> ExecutionHookRegistration
    where
        I: IntoIterator<Item = Arc<dyn ExecutionHookParticipant>>,
    {
        self.handle
            .execution_hooks()
            .register_live(Some(audience), participants.into_iter().collect())
    }

    /// Registers a skills directory and enables the builtin `load_skill` tool.
    ///
    /// Additive: calling this again adds a second root rather than replacing
    /// the first. Register the most specific root first — a name two roots
    /// both define resolves to the one registered earlier, and the shadowed
    /// skill is outranked rather than discarded, so
    /// [`unregister_skills_dir`](Self::unregister_skills_dir) on the winner
    /// brings it back.
    ///
    /// A root already registered is *reloaded in place*, keeping the
    /// precedence it had: one entry per directory, so one unregister always
    /// suffices to drop it.
    ///
    /// Nothing is registered when the root fails to load.
    pub fn register_skills_dir(&self, path: impl AsRef<Path>) -> Result<(), SkillLoadError> {
        self.handle
            .register_skill_roots(vec![skill::SkillRoot::load(path)?]);
        Ok(())
    }

    /// Registers several skills directories at once, strongest first.
    ///
    /// Equivalent to calling [`register_skills_dir`](Self::register_skills_dir)
    /// for each in order, with one difference that matters to a host: the call
    /// is atomic. Every root is loaded and validated before any is committed,
    /// so an `Err` leaves the runtime exactly as it was and names the root
    /// that failed. Fixing that root and calling again is a clean retry rather
    /// than a second, overlapping registration.
    ///
    /// Within a single root a repeated name is still
    /// [`SkillLoadError::DuplicateSkillName`]: across roots it is layering,
    /// inside one root it is a mistake.
    pub fn register_skills_dirs<I, P>(&self, paths: I) -> Result<(), SkillLoadError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let roots = paths
            .into_iter()
            .map(skill::SkillRoot::load)
            .collect::<Result<Vec<_>, _>>()?;
        self.handle.register_skill_roots(roots);
        Ok(())
    }

    /// Drops every skill registered from `path`, reporting whether the root
    /// was there.
    ///
    /// The inverse of [`register_skills_dir`](Self::register_skills_dir), for
    /// a host that outlives the thing a root belongs to — an editor server
    /// closing one repository while other repositories keep running on the
    /// same runtime. A dropped skill is unreachable, not merely unlisted:
    /// `load_skill` refuses it, and it leaves the model-facing skill list.
    /// A name this root had shadowed resolves to the weaker root again.
    ///
    /// The root is matched by canonical path, so a path spelled differently
    /// than at registration still names it; a root whose directory has since
    /// been deleted is matched by the exact path that registered it.
    ///
    /// Dropping the last root also withdraws the `load_skill` tool, which the
    /// next registration restores.
    pub fn unregister_skills_dir(&self, path: impl AsRef<Path>) -> bool {
        self.handle.unregister_skill_roots([path])
    }

    /// Drops several skills directories at once, reporting whether *any* of
    /// them was registered.
    ///
    /// Every path that names a registered root is dropped regardless of the
    /// others, so a host closing a workspace can pass the same list it
    /// registered without first checking which roots still exist.
    pub fn unregister_skills_dirs<I, P>(&self, paths: I) -> bool
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        self.handle.unregister_skill_roots(paths)
    }

    /// Every loaded skill, name-ordered, with its description, source path and
    /// registered root but not its body.
    ///
    /// Shadowed skills are left out: this is what a name resolves to today.
    pub fn skills(&self) -> Vec<SkillInfo> {
        self.handle.skills()
    }

    /// How each configured MCP server fared while the runtime was built.
    ///
    /// Empty when none were configured, or when the runtime came from
    /// [`build`](RuntimeBuilder::build), which refuses to be given any.
    /// A failed server is present with its error rather than absent: a host
    /// telling a user which tools they have needs to name what is missing.
    pub fn mcp_servers(&self) -> &[McpServerSummary] {
        &self.mcp_servers
    }

    /// Returns a lead-privileged task-board view for `namespace`.
    ///
    /// The namespace is an opaque store key; no directory is created. Reads are
    /// live and every mutation passes through the same validation and
    /// transactional store path as the builtin task tools.
    pub fn task_board(&self, namespace: impl AsRef<Path>) -> TaskBoard {
        TaskBoard::lead(self.handle.clone(), namespace.as_ref().to_path_buf())
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
        self.spawn_with_config_and_audience(name, model, config, None)
    }

    /// Spawns a new agent in an ephemeral tool audience.
    pub fn spawn_with_config_for_audience(
        &self,
        name: impl Into<String>,
        model: ModelInfo,
        config: AgentConfig,
        audience: crate::tool::ToolAudience,
    ) -> Result<Agent, RuntimeError> {
        self.spawn_with_config_and_audience(name, model, config, Some(audience))
    }

    fn spawn_with_config_and_audience(
        &self,
        name: impl Into<String>,
        model: ModelInfo,
        config: AgentConfig,
        audience: Option<crate::tool::ToolAudience>,
    ) -> Result<Agent, RuntimeError> {
        Agent::new(
            self.handle.with_tool_audience(audience),
            model.id,
            model.context_window,
            name.into(),
            config,
            self.provider_registry
                .read()
                .expect("provider registry poisoned")
                .get_provider(Some(&model.provider))
                .ok_or_else(|| RuntimeError::ProviderNotFound(Some(model.provider.clone())))?,
            AgentSpawnOptions::default(),
        )
    }

    /// Restores a previously persisted agent by identifier.
    pub fn resume_agent(&self, agent_id: &str) -> Result<Agent, RuntimeError> {
        self.resume_agent_with_audience(agent_id, None)
    }

    /// Restores a persisted agent in the supplied live tool audience.
    pub fn resume_agent_for_audience(
        &self,
        agent_id: &str,
        audience: crate::tool::ToolAudience,
    ) -> Result<Agent, RuntimeError> {
        self.resume_agent_with_audience(agent_id, Some(audience))
    }

    fn resume_agent_with_audience(
        &self,
        agent_id: &str,
        audience: Option<crate::tool::ToolAudience>,
    ) -> Result<Agent, RuntimeError> {
        let Some(state) = self.handle.store().load_agent(agent_id)? else {
            return Err(RuntimeError::Store(format!(
                "No persisted agent with id '{agent_id}'"
            )));
        };
        let provider = self
            .provider_registry
            .read()
            .expect("provider registry poisoned")
            .get_provider(Some(&state.record.provider_id))
            .ok_or_else(|| {
                RuntimeError::ProviderNotFound(Some(state.record.provider_id.clone()))
            })?;
        Agent::from_loaded(self.handle.with_tool_audience(audience), state, provider)
    }

    /// Restores every persisted agent that belongs to the provided runtime identifier.
    pub fn resume(&self, runtime_identifier: &str) -> Result<Vec<Agent>, RuntimeError> {
        self.resume_with_audience(runtime_identifier, None)
    }

    /// Restores every persisted agent under an ephemeral tool audience.
    pub fn resume_for_audience(
        &self,
        runtime_identifier: &str,
        audience: crate::tool::ToolAudience,
    ) -> Result<Vec<Agent>, RuntimeError> {
        self.resume_with_audience(runtime_identifier, Some(audience))
    }

    fn resume_with_audience(
        &self,
        runtime_identifier: &str,
        audience: Option<crate::tool::ToolAudience>,
    ) -> Result<Vec<Agent>, RuntimeError> {
        let states = self
            .handle
            .store()
            .list_agents_by_runtime(runtime_identifier)?;
        let mut agents = Vec::new();
        for state in states {
            let provider = self
                .provider_registry
                .read()
                .expect("provider registry poisoned")
                .get_provider(Some(&state.record.provider_id))
                .ok_or_else(|| {
                    RuntimeError::ProviderNotFound(Some(state.record.provider_id.clone()))
                })?;
            let agent = Agent::from_loaded(
                self.handle.with_tool_audience(audience.clone()),
                state,
                provider,
            )?;
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
                        created_at: state.created_at,
                        updated_at: state.updated_at,
                    })
                    .collect()
            })
    }

    /// Removes a persisted agent and everything stored under it.
    ///
    /// Deleting the record without its memory would leave a row that
    /// [`resume`](Self::resume) refuses with "missing persisted memory", so
    /// this removes both. It does not stop a live [`Agent`] already holding
    /// that id — an agent in memory keeps running, and persists itself again
    /// on its next write.
    pub fn delete_agent(&self, agent_id: &str) -> Result<(), RuntimeError> {
        self.handle.store().delete_agent(agent_id)
    }

    /// Restores every persisted agent known to the runtime store.
    pub fn resume_all(&self) -> Result<Vec<Agent>, RuntimeError> {
        let states = self.handle.store().list_agents()?;
        let mut agents = Vec::new();
        for state in states {
            let provider = self
                .provider_registry
                .read()
                .expect("provider registry poisoned")
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
        self.provider_registry
            .read()
            .expect("provider registry poisoned")
            .descriptors()
    }

    /// Mints the selected provider's configuration into an independent session scope.
    ///
    /// `None` selects the runtime's default provider. The operation is local and
    /// synchronous: it allocates provider-owned scope state but does not open or
    /// warm a connection. The returned [`ProviderSessionScope`] implements
    /// [`Provider`] and can be passed directly to
    /// [`RuntimeBuilder::with_provider_instance`]. Ordinary clones share the
    /// returned scope; call [`Provider::fresh_session_scope`] again to split it.
    pub fn fresh_provider_session_scope(
        &self,
        provider: Option<&ProviderId>,
    ) -> Result<ProviderSessionScope, RuntimeError> {
        let source = {
            self.provider_registry
                .read()
                .expect("provider registry poisoned")
                .get_provider(provider)
        }
        .ok_or_else(|| RuntimeError::ProviderNotFound(provider.cloned()))?;
        let expected = source.descriptor().id;
        let scope = source
            .fresh_session_scope()
            .map_err(RuntimeError::FailedToCreateProviderSessionScope)?;
        let actual = scope.descriptor().id;

        if actual != expected {
            return Err(RuntimeError::ProviderSessionScopeIdentityMismatch { expected, actual });
        }

        Ok(scope)
    }

    /// The Responses transport this runtime chose for every request it makes,
    /// or `None` when it left the choice to each request's own options — which
    /// is HTTP+SSE unless a host set otherwise.
    ///
    /// The reader for
    /// [`RuntimeBuilder::with_responses_transport`](crate::runtime::RuntimeBuilder::with_responses_transport).
    /// A transport is otherwise the one piece of a runtime's configuration
    /// nothing can observe: a registered tool shows up in
    /// [`tools`](Self::tools), a provider in [`providers`](Self::providers),
    /// but a transport reaches only the requests the runtime sends. That makes
    /// the wiring between a host's choice and this runtime untestable except by
    /// running a turn against a provider that records what it was handed — and
    /// leaves a host that wants to report its own configuration with no way to
    /// ask.
    pub fn responses_transport(&self) -> Option<crate::provider::ResponsesTransport> {
        self.provider_registry
            .read()
            .expect("provider registry poisoned")
            .responses_transport()
    }

    /// Registers a builtin provider from an API key.
    pub fn register_provider(
        &mut self,
        id: BuiltinProvider,
        api_key: impl Into<String>,
    ) -> Result<(), String> {
        self.provider_registry
            .write()
            .expect("provider registry poisoned")
            .register_builtin_provider(id, api_key)
    }

    /// Registers the local Ollama provider using its default OpenAI-compatible endpoint.
    pub fn register_ollama(&mut self) {
        self.provider_registry
            .write()
            .expect("provider registry poisoned")
            .register_ollama();
    }

    /// Registers the local LM Studio provider using its default OpenAI-compatible endpoint.
    pub fn register_lmstudio(&mut self) {
        self.provider_registry
            .write()
            .expect("provider registry poisoned")
            .register_lmstudio();
    }

    /// Registers any endpoint speaking the OpenAI `chat/completions` wire.
    ///
    /// `id` is the name this runtime will know the provider by. Almost every
    /// OpenAI-compatible endpoint — DeepSeek, Groq, Together, Fireworks,
    /// Mistral, xAI, vLLM, llama.cpp — serves this wire and not OpenAI's own
    /// `v1/responses`.
    ///
    /// ```rust,no_run
    /// # let mut runtime = mentra::Runtime::empty_builder().build().unwrap();
    /// runtime.register_openai_compatible(
    ///     "groq",
    ///     "https://api.groq.com/openai/",
    ///     std::env::var("GROQ_API_KEY").unwrap(),
    /// );
    /// ```
    pub fn register_openai_compatible(
        &mut self,
        id: impl Into<crate::provider::ProviderId>,
        base_url: impl AsRef<str>,
        api_key: impl Into<String>,
    ) {
        self.provider_registry
            .write()
            .expect("provider registry poisoned")
            .register_provider_instance(
                crate::provider::openai_compatible::OpenAiCompatibleProvider::new(
                    id, base_url, api_key,
                ),
            );
    }

    /// Registers an OpenAI-compatible endpoint that wants no credentials, such
    /// as a local vLLM or llama.cpp server.
    pub fn register_openai_compatible_without_credentials(
        &mut self,
        id: impl Into<crate::provider::ProviderId>,
        base_url: impl AsRef<str>,
    ) {
        self.provider_registry
            .write()
            .expect("provider registry poisoned")
            .register_provider_instance(
                crate::provider::openai_compatible::OpenAiCompatibleProvider::without_credentials(
                    id, base_url,
                ),
            );
    }

    /// Registers a custom runtime provider implementation.
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
        self.provider_registry
            .write()
            .expect("provider registry poisoned")
            .register_provider_instance(provider);
    }

    /// Registers a provider-core instance built from `mentra::provider_core`.
    ///
    /// Use this when you want Mentra's runtime with a customized provider
    /// definition, such as a custom OpenAI-compatible or Anthropic-compatible
    /// base URL.
    pub fn register_registered_provider<P>(&mut self, provider: P)
    where
        P: mentra_provider::Provider + 'static,
    {
        self.provider_registry
            .write()
            .expect("provider registry poisoned")
            .register_registered_provider(provider);
    }

    /// Lists models for a specific provider, or the default provider when omitted.
    pub async fn list_models(
        &self,
        provider: Option<&ProviderId>,
    ) -> Result<Vec<ModelInfo>, RuntimeError> {
        let provider = self
            .provider_registry
            .read()
            .expect("provider registry poisoned")
            .get_provider(provider)
            .ok_or_else(|| RuntimeError::ProviderNotFound(provider.cloned()))?;

        provider
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
            .read()
            .expect("provider registry poisoned")
            .get_provider(Some(&provider))
            .is_none()
        {
            return Err(RuntimeError::ProviderNotFound(Some(provider)));
        }

        match selector {
            // A named model still gets looked up, because the listing is where
            // metadata the caller cannot supply lives — `context_window` above
            // all, which decides the compaction threshold. Synthesizing the
            // `ModelInfo` from the id alone left every pinned `--model`
            // resolving to an unknown window, so window-relative compaction
            // silently applied to none of them.
            //
            // The lookup is best-effort in both directions: a provider that
            // cannot list, fails to, or simply does not name this id still
            // resolves, because a model id the caller pinned is a fact about
            // their intent and not a claim the listing has to confirm.
            ModelSelector::Id(id) => Ok(self
                .listed_model(&provider, &id)
                .await
                .unwrap_or_else(|| ModelInfo::new(id, provider))),
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

    /// Looks `id` up in a provider's listing, or `None` if it cannot be found
    /// there for any reason.
    async fn listed_model(&self, provider: &ProviderId, id: &str) -> Option<ModelInfo> {
        let lists_models = self
            .provider_registry
            .read()
            .expect("provider registry poisoned")
            .get_provider(Some(provider))
            .is_some_and(|provider| provider.capabilities().supports_model_listing);
        if !lists_models {
            return None;
        }

        self.list_models(Some(provider))
            .await
            .ok()?
            .into_iter()
            .find(|model| model.id == id)
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
    ///
    /// Convenience wrapper around [`create_session_full`](Self::create_session_full) that
    /// passes `None` for `project_id`.
    pub fn create_session_with_config(
        &self,
        name: impl Into<String>,
        model: ModelInfo,
        config: AgentConfig,
    ) -> Result<Session, RuntimeError> {
        self.create_session_full(name, model, config, None)
    }

    /// Creates a new session with full control over how it is scoped and
    /// persisted.
    ///
    /// The reason this exists next to
    /// [`create_session_full`](Self::create_session_full): a runtime's
    /// identifier is otherwise fixed when the runtime is built, so every
    /// session minted on one runtime carries the same tag and
    /// [`list_persisted_agents`](Self::list_persisted_agents) cannot tell them
    /// apart. A host serving several workspaces from one runtime — an editor
    /// with more than one project open — needs each session's rows tagged with
    /// the workspace they belong to.
    pub fn create_session_with_options(
        &self,
        name: impl Into<String>,
        model: ModelInfo,
        options: SessionOptions,
    ) -> Result<Session, RuntimeError> {
        self.build_session(name.into(), model, options)
    }

    /// Creates a new session wrapping a freshly spawned agent with explicit config and
    /// an optional project identifier.
    ///
    /// The `project_id` is threaded into the automatically attached
    /// [`SessionPermissionHandle`](crate::SessionPermissionHandle), so project
    /// permission rules use the runtime's [`PermissionRuleStore`] immediately.
    pub fn create_session_full(
        &self,
        name: impl Into<String>,
        model: ModelInfo,
        config: AgentConfig,
        project_id: Option<String>,
    ) -> Result<Session, RuntimeError> {
        self.build_session(
            name.into(),
            model,
            SessionOptions {
                config,
                policy: None,
                tool_audience: None,
                project_id,
                runtime_identifier: None,
            },
        )
    }

    /// The runtime's hook list plus a bridge into one session's event channel.
    ///
    /// Every session is built on its own [`RuntimeHandle`] clone, and each
    /// `with_*` step rebuilds the handle's [`MemoryEngine`] from the hook list
    /// the clone carries — so a hook appended here fires only for the agents
    /// that run on this session's handle: the session's own agent and the
    /// subagents it spawns. That containment is what makes registering the
    /// bridge correct at all; on the runtime's shared list it would deliver
    /// every agent's memory activity to whichever session installed it first.
    ///
    /// [`MemoryEngine`]: crate::memory::MemoryEngine
    fn session_scoped_hooks(&self, event_tx: &broadcast::Sender<SessionEvent>) -> RuntimeHooks {
        self.handle
            .execution
            .hooks
            .clone()
            .with_hook(SessionHookBridge::new(event_tx.clone()))
    }

    /// Derives every piece of live session scope before an agent is registered.
    ///
    /// The order is intentional: the session event hook and complete policy are
    /// applied before registration, then runtime identity and tool audience
    /// refine persistence and live visibility without changing that policy.
    /// [`Session::new_with_parts`] installs the outer permission authorizer only
    /// after `Agent::new` or `Agent::from_loaded` exposes the stable agent id,
    /// preserving every scoped handle service assembled here.
    fn session_scoped_handle(
        &self,
        event_tx: &broadcast::Sender<SessionEvent>,
        policy: Option<RuntimePolicy>,
        runtime_identifier: Option<Arc<str>>,
        tool_audience: Option<crate::tool::ToolAudience>,
    ) -> RuntimeHandle {
        let handle = self.handle.with_hooks(self.session_scoped_hooks(event_tx));
        let handle = match policy {
            Some(policy) => handle.with_policy(policy),
            None => handle,
        };
        let handle = match runtime_identifier {
            Some(identifier) => handle.with_runtime_identifier(identifier),
            None => handle,
        };
        handle.with_tool_audience(tool_audience)
    }

    fn build_session(
        &self,
        name: String,
        model: ModelInfo,
        options: SessionOptions,
    ) -> Result<Session, RuntimeError> {
        let SessionOptions {
            config,
            policy,
            tool_audience,
            project_id,
            runtime_identifier,
        } = options;
        let session_id = SessionId::new();
        let metadata = SessionMetadata::new(session_id.clone(), &name, &model.id);
        let (event_tx, _) = broadcast::channel(512);
        let pending_permissions = PendingPermissionStore::new();
        let session_handle =
            self.session_scoped_handle(&event_tx, policy, runtime_identifier, tool_audience);
        let provider = self
            .provider_registry
            .read()
            .expect("provider registry poisoned")
            .get_provider(Some(&model.provider))
            .ok_or_else(|| RuntimeError::ProviderNotFound(Some(model.provider.clone())))?;
        let agent = Agent::new(
            session_handle,
            model.id.clone(),
            model.context_window,
            name.clone(),
            config,
            provider,
            AgentSpawnOptions::default(),
        )?;
        let mut session = Session::new_with_parts(
            session_id.clone(),
            metadata,
            agent,
            event_tx,
            pending_permissions,
            project_id,
        );

        // Emit the initial SessionStarted event.
        let started = SessionEvent::SessionStarted { session_id };
        // Subscribe briefly just to ensure the event is broadcast.
        let _rx = session.subscribe();
        // Use the internal emit path via a helper on Session.
        session.emit_started(started);

        Ok(session)
    }

    /// Resumes a previously persisted agent and wraps it in a session.
    ///
    /// Convenience wrapper around [`resume_session_with_project`](Self::resume_session_with_project)
    /// that passes `None` for `project_id`.
    pub fn resume_session(&self, agent_id: &str) -> Result<Session, RuntimeError> {
        self.resume_session_with_options(agent_id, SessionResumeOptions::default())
    }

    /// Resumes a previously persisted agent, wraps it in a session, and associates
    /// the session with an optional project identifier.
    ///
    /// The `project_id` is threaded into the automatically attached
    /// [`SessionPermissionHandle`](crate::SessionPermissionHandle), so project
    /// permission rules use the current runtime store immediately.
    pub fn resume_session_with_project(
        &self,
        agent_id: &str,
        project_id: Option<String>,
    ) -> Result<Session, RuntimeError> {
        self.resume_session_with_options(
            agent_id,
            SessionResumeOptions {
                project_id,
                policy: None,
                tool_audience: None,
            },
        )
    }

    /// Resumes a persisted agent with live, non-persisted session scope.
    pub fn resume_session_with_options(
        &self,
        agent_id: &str,
        options: SessionResumeOptions,
    ) -> Result<Session, RuntimeError> {
        let SessionResumeOptions {
            project_id,
            policy,
            tool_audience,
        } = options;
        let session_id = SessionId::new();
        let (event_tx, _) = broadcast::channel(512);
        let pending_permissions = PendingPermissionStore::new();
        let session_handle = self.session_scoped_handle(&event_tx, policy, None, tool_audience);
        let Some(state) = self.handle.store().load_agent(agent_id)? else {
            return Err(RuntimeError::Store(format!(
                "No persisted agent with id '{agent_id}'"
            )));
        };
        let provider = self
            .provider_registry
            .read()
            .expect("provider registry poisoned")
            .get_provider(Some(&state.record.provider_id))
            .ok_or_else(|| {
                RuntimeError::ProviderNotFound(Some(state.record.provider_id.clone()))
            })?;
        let agent = Agent::from_loaded(session_handle, state, provider)?;
        let metadata = SessionMetadata::new(session_id.clone(), agent.name(), agent.model());
        let session = Session::new_with_parts(
            session_id,
            metadata,
            agent,
            event_tx,
            pending_permissions,
            project_id,
        );
        Ok(session)
    }
}
