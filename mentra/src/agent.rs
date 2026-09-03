mod compact;
mod config;
mod events;
mod lifecycle;
mod pending;
mod pending_block;
mod round_strategy;
mod runner;
mod snapshot;
mod steering;
mod subagent;
mod task_state;
mod team;
mod terminal_output;
#[cfg(test)]
mod tests;
mod wait;

use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, watch};

use crate::{
    ContentBlock, Message,
    background::BackgroundNotification,
    error::RuntimeError,
    memory::journal::{AgentMemory, AgentMemoryState as MemoryState},
    provider::{Provider, ProviderId, ToolChoice},
    runtime::{
        LoadedAgentState, RuntimeIntrinsicTool, TaskItem,
        handle::{AgentExecutionConfig, AgentObserver, RuntimeHandle},
    },
    team::TeamMessage,
    transcript::{DelegationArtifact, DelegationEdge, TranscriptItem},
};

pub(crate) use team::parse_task_input;

pub use config::{
    AgentConfig, AutoCompactTrigger, CompactionConfig, ContextCompactionConfig, MemoryConfig,
    ProjectedToolResultBudget, TaskConfig, TeamAutonomyConfig, TeamConfig, ToolProfile,
    ToolResultPagingConfig, WorkspaceConfig,
};
pub use events::{
    AgentEvent, AgentSnapshot, AgentStatus, CompactionDetails, CompactionTrigger,
    ContextCompactionDetails, ContextCompactionTrigger, ElidedToolResult, PendingToolUseSummary,
    RequestToolResultElision, RequestToolResultElisionPolicy, SpawnedAgentStatus,
    SpawnedAgentSummary, ToolResultContentKind, ToolResultElisionAction,
};
pub use pending::PendingAssistantTurn;
pub use round_strategy::{
    ReasoningChange, RoundAdjustment, RoundBoundary, RoundContext, RoundDecision, RoundStrategy,
    RoundToolResult,
};
use runner::TurnRunner;
pub use steering::{QueueMode, SteeringHandle};
pub use subagent::DisposableSubagentTemplate;
use terminal_output::TerminalToolGate;
pub use terminal_output::{
    FinalOutput, TerminalOutputDecision, TerminalOutputReservation, TerminalOutputSpec,
};
pub use wait::{AgentWaitFuture, AgentWaitHandle};

static NEXT_AGENT_ID: AtomicU64 = AtomicU64::new(1);

/// Running or persisted agent managed by a [`crate::Runtime`].
pub struct Agent {
    id: String,
    runtime: RuntimeHandle,
    model: String,
    /// How many tokens this agent's model accepts, when the provider said so.
    ///
    /// Not persisted: it belongs to the model listing, not to the agent, and a
    /// resumed agent that has not been handed a fresh `ModelInfo` falls back to
    /// the absolute compaction threshold rather than guessing a window.
    context_window: Option<usize>,
    provider_id: ProviderId,
    name: String,
    config: AgentConfig,
    memory: AgentMemory,
    tasks: Vec<TaskItem>,
    rounds_since_task: usize,
    event_bus: AgentEventBus,
    snapshot: Arc<Mutex<AgentSnapshot>>,
    snapshot_tx: watch::Sender<AgentSnapshot>,
    provider: Arc<dyn Provider>,
    hidden_tools: HashSet<String>,
    terminal_tool_gate: Arc<Mutex<Option<TerminalToolGate>>>,
    max_rounds: Option<usize>,
    inflight_background_notifications: Vec<BackgroundNotification>,
    inflight_team_messages: Vec<TeamMessage>,
    steering: SteeringHandle,
    inflight_steer: Vec<Vec<ContentBlock>>,
    inflight_follow_up: Vec<Vec<ContentBlock>>,
    teammate_identity: Option<TeammateIdentity>,
    idle_requested: bool,
    current_run_id: Option<String>,
    /// Full texts of results this agent received paged, keyed by
    /// `tool_use_id` — the backing store for `read_tool_result`. Empty and
    /// unused unless `config.tool_result_paging` is set.
    paged_tool_results: crate::tool::paging::PagedToolResults,
    /// Exact-agent registration for `read_tool_result`, retained only while
    /// this paging agent is live.
    _tool_result_reader: Option<crate::tool::AgentToolRegistration>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TeammateIdentity {
    pub(crate) role: String,
    pub(crate) lead: String,
}

#[derive(Default)]
pub(crate) struct AgentSpawnOptions {
    pub(crate) hidden_tools: HashSet<String>,
    pub(crate) max_rounds: Option<usize>,
    pub(crate) teammate_identity: Option<TeammateIdentity>,
}

type AgentEventTap = Arc<dyn Fn(&AgentEvent) + Send + Sync>;

#[derive(Default)]
struct AgentEventTapRegistry {
    next_id: u64,
    taps: Vec<(u64, AgentEventTap)>,
}

/// Keeps one agent's synchronous event tap registered.
///
/// Dropping it stops future observation and waits for any callback already in
/// flight, so it must outlive the run being observed — binding it to `_` drops
/// the guard before observation can continue.
///
/// Because drop waits, do not drop a guard while holding a lock or other
/// resource that an in-flight callback needs.
#[must_use = "dropping the guard unregisters the tap and may wait for an in-flight callback"]
pub struct AgentEventTapGuard {
    registry: Arc<Mutex<AgentEventTapRegistry>>,
    dispatch: Arc<Mutex<()>>,
    id: u64,
}

#[derive(Clone)]
pub(crate) struct AgentEventBus {
    tx: broadcast::Sender<AgentEvent>,
    taps: Arc<Mutex<AgentEventTapRegistry>>,
    /// Serializes tap callbacks and the matching broadcast send into one
    /// occurrence order. Guard drop takes the same gate before unregistering,
    /// making its return a quiescence boundary for already-started callbacks.
    dispatch: Arc<Mutex<()>>,
}

impl AgentEventBus {
    fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            tx,
            taps: Arc::new(Mutex::new(AgentEventTapRegistry::default())),
            dispatch: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn send(&self, event: AgentEvent) {
        let _dispatch = self
            .dispatch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let taps = {
            let registry = self.taps.lock().expect("agent event tap registry poisoned");
            registry
                .taps
                .iter()
                .map(|(_, tap)| Arc::clone(tap))
                .collect::<Vec<_>>()
        };
        for tap in taps {
            tap(&event);
        }
        let _ = self.tx.send(event);
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.tx.subscribe()
    }

    pub(crate) fn register_tap(
        &self,
        tap: impl Fn(&AgentEvent) + Send + Sync + 'static,
    ) -> AgentEventTapGuard {
        let mut registry = self.taps.lock().expect("agent event tap registry poisoned");
        let id = registry.next_id;
        registry.next_id += 1;
        registry.taps.push((id, Arc::new(tap)));
        AgentEventTapGuard {
            registry: Arc::clone(&self.taps),
            dispatch: Arc::clone(&self.dispatch),
            id,
        }
    }
}

impl Drop for AgentEventTapGuard {
    fn drop(&mut self) {
        let removed = {
            let _dispatch = self
                .dispatch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut registry = self
                .registry
                .lock()
                .expect("agent event tap registry poisoned");
            registry
                .taps
                .iter()
                .position(|(tap_id, _)| *tap_id == self.id)
                .map(|index| registry.taps.remove(index).1)
        };

        // A callback owns arbitrary user captures. Destroy them only after the
        // dispatch and registry locks are released: a capture may contain a
        // second guard for this same bus, and its destructor must be able to
        // acquire both locks without recursing into them.
        drop(removed);
    }
}

impl Agent {
    pub(crate) fn new(
        runtime: RuntimeHandle,
        model: String,
        context_window: Option<usize>,
        name: String,
        config: AgentConfig,
        provider: Arc<dyn Provider>,
        options: AgentSpawnOptions,
    ) -> Result<Self, RuntimeError> {
        let AgentSpawnOptions {
            hidden_tools,
            max_rounds,
            teammate_identity,
        } = options;
        let store = runtime.store();
        let agent_id = format!(
            "agent-{:x}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            NEXT_AGENT_ID.fetch_add(1, Ordering::Relaxed)
        );
        let memory = AgentMemory::new(agent_id.clone(), store.clone(), MemoryState::default());
        let event_bus = AgentEventBus::new(256);
        let memory_view = memory.snapshot_view();
        let snapshot = AgentSnapshot {
            history_len: memory_view.history_len,
            current_text: memory_view.current_text,
            pending_tool_uses: memory_view.pending_tool_uses,
            ..Default::default()
        };
        let snapshot = Arc::new(Mutex::new(snapshot));
        let (snapshot_tx, _) =
            watch::channel(snapshot.lock().expect("agent snapshot poisoned").clone());
        let mut agent = Self {
            id: agent_id,
            runtime,
            model,
            context_window,
            provider_id: provider.descriptor().id,
            name,
            config,
            memory,
            tasks: Vec::new(),
            rounds_since_task: 0,
            event_bus,
            snapshot,
            snapshot_tx,
            provider,
            hidden_tools,
            terminal_tool_gate: Arc::new(Mutex::new(None)),
            max_rounds,
            inflight_background_notifications: Vec::new(),
            inflight_team_messages: Vec::new(),
            steering: SteeringHandle::new(),
            inflight_steer: Vec::new(),
            inflight_follow_up: Vec::new(),
            teammate_identity,
            idle_requested: false,
            current_run_id: None,
            paged_tool_results: Default::default(),
            _tool_result_reader: None,
        };
        agent
            .runtime
            .store()
            .create_agent(&agent.persisted_record(), agent.memory.state())?;
        let execution_config = AgentExecutionConfig {
            name: agent.name.clone(),
            team_dir: agent.config.team.team_dir.clone(),
            tasks_dir: agent.config.task.tasks_dir.clone(),
            base_dir: agent.config.workspace.base_dir.clone(),
            memory_tool_search_limit: agent.config.memory.tool_search_limit,
            auto_route_shell: agent.config.workspace.auto_route_shell,
            is_teammate: agent.teammate_identity.is_some(),
        };
        let observer = AgentObserver {
            events: agent.event_bus.clone(),
            snapshot_tx: agent.snapshot_tx.clone(),
            snapshot: Arc::clone(&agent.snapshot),
        };
        agent
            .runtime
            .register_agent(&agent.id, &agent.name, execution_config, &observer)?;
        agent.register_tool_result_pager();
        agent.refresh_tasks_from_disk()?;
        Ok(agent)
    }

    pub(crate) fn from_loaded(
        runtime: RuntimeHandle,
        mut state: LoadedAgentState,
        provider: Arc<dyn Provider>,
    ) -> Result<Self, RuntimeError> {
        let runtime = runtime.rebind_persisted_runtime_identifier(Arc::<str>::from(
            state.record.runtime_identifier.as_str(),
        ));
        let mut memory = AgentMemory::new(state.record.id.clone(), runtime.store(), state.memory);
        let recovery = memory.recover()?;
        if recovery.interrupted {
            state.record.status = AgentStatus::Interrupted;
            runtime.store().update_run_state(
                recovery
                    .interrupted_run_id
                    .as_deref()
                    .expect("recovery should include run id"),
                "interrupted",
                Some("recovered after interruption"),
            )?;
            runtime.store().save_agent_record(&state.record)?;
        }
        let memory_view = memory.snapshot_view();
        let snapshot = AgentSnapshot {
            status: state.record.status.clone(),
            history_len: memory_view.history_len,
            current_text: memory_view.current_text,
            pending_tool_uses: memory_view.pending_tool_uses,
            pending_team_messages: 0,
            subagents: state.record.subagents.clone(),
            ..Default::default()
        };
        let snapshot = Arc::new(Mutex::new(snapshot));
        let (snapshot_tx, _) =
            watch::channel(snapshot.lock().expect("agent snapshot poisoned").clone());
        let event_bus = AgentEventBus::new(256);
        let mut agent = Self {
            id: state.record.id.clone(),
            runtime,
            model: state.record.model.clone(),
            // The persisted record carries a model id and nothing about the
            // model; a host that wants the window back calls `set_model`.
            context_window: None,
            provider_id: state.record.provider_id.clone(),
            name: state.record.name.clone(),
            config: state.record.config.clone(),
            memory,
            tasks: Vec::new(),
            rounds_since_task: state.record.rounds_since_task,
            event_bus,
            snapshot,
            snapshot_tx,
            provider,
            hidden_tools: state.record.hidden_tools,
            terminal_tool_gate: Arc::new(Mutex::new(None)),
            max_rounds: state.record.max_rounds,
            inflight_background_notifications: Vec::new(),
            inflight_team_messages: Vec::new(),
            steering: SteeringHandle::new(),
            inflight_steer: Vec::new(),
            inflight_follow_up: Vec::new(),
            teammate_identity: state.record.teammate_identity,
            idle_requested: state.record.idle_requested,
            current_run_id: None,
            paged_tool_results: Default::default(),
            _tool_result_reader: None,
        };
        let execution_config = AgentExecutionConfig {
            name: agent.name.clone(),
            team_dir: agent.config.team.team_dir.clone(),
            tasks_dir: agent.config.task.tasks_dir.clone(),
            base_dir: agent.config.workspace.base_dir.clone(),
            memory_tool_search_limit: agent.config.memory.tool_search_limit,
            auto_route_shell: agent.config.workspace.auto_route_shell,
            is_teammate: agent.teammate_identity.is_some(),
        };
        let observer = AgentObserver {
            events: agent.event_bus.clone(),
            snapshot_tx: agent.snapshot_tx.clone(),
            snapshot: Arc::clone(&agent.snapshot),
        };
        agent
            .runtime
            .register_agent(&agent.id, &agent.name, execution_config, &observer)?;
        agent.register_tool_result_pager();
        agent.refresh_tasks_from_disk()?;
        Ok(agent)
    }

    /// Returns the agent's display name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the stable persisted agent identifier.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the model identifier used by the agent.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Returns how many tokens this agent's model accepts, when known.
    ///
    /// `None` means the provider's listing did not say and no host has: it is
    /// unknown, not unlimited.
    pub fn context_window(&self) -> Option<usize> {
        self.context_window
    }

    /// Updates the model and provider used for future turns, then persists the
    /// new agent record so resumed sessions continue with the same setting.
    pub fn set_model(&mut self, model: crate::ModelInfo) -> Result<(), RuntimeError> {
        let provider = self
            .runtime
            .get_provider(Some(&model.provider))
            .ok_or_else(|| RuntimeError::ProviderNotFound(Some(model.provider.clone())))?;
        self.model = model.id;
        self.context_window = model.context_window;
        self.provider_id = provider.descriptor().id;
        self.provider = provider;
        self.persist_agent_record()
    }

    /// Updates the reasoning options requested on future turns, then persists the
    /// agent record so resumed sessions continue with the same setting.
    ///
    /// Mirrors [`set_model`](Self::set_model): a stateful override threaded into
    /// every subsequent model request (the runner reads
    /// `config.provider_request_options.reasoning` live). It composes with
    /// `set_model` for **per-phase tiering** — e.g. run the gather rounds at a low
    /// reasoning effort, then raise the effort (and switch to a stronger model) for
    /// a final synthesis turn on the same agent, without re-spawning and losing the
    /// gathered context. `None` clears any configured reasoning, restoring the
    /// provider's default effort.
    pub fn set_reasoning(
        &mut self,
        reasoning: Option<crate::provider::ReasoningOptions>,
    ) -> Result<(), RuntimeError> {
        self.config.provider_request_options.reasoning = reasoning;
        self.persist_agent_record()
    }

    /// Returns the reasoning options future turns will be sent with.
    ///
    /// The reader for [`set_reasoning`](Self::set_reasoning). A picker that
    /// cannot show which effort a session is on has to keep its own copy and
    /// hope the two never diverge.
    pub fn reasoning(&self) -> Option<&crate::provider::ReasoningOptions> {
        self.config.provider_request_options.reasoning.as_ref()
    }

    /// Renames the agent and persists the new name.
    pub fn set_name(&mut self, name: impl Into<String>) -> Result<(), RuntimeError> {
        self.name = name.into();
        self.persist_agent_record()
    }

    /// Returns the effective agent configuration.
    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    /// Returns this live agent's ephemeral tool audience, if any.
    pub fn tool_audience(&self) -> Option<&crate::tool::ToolAudience> {
        self.runtime.tool_audience()
    }

    /// Returns the committed transcript history.
    pub fn history(&self) -> &[Message] {
        self.memory.history()
    }

    /// Returns the canonical transcript items stored for this agent.
    pub fn transcript(&self) -> &crate::AgentTranscript {
        self.memory.transcript()
    }

    /// The transcript entry the next turn will continue from.
    pub fn leaf(&self) -> Option<&crate::transcript::EntryId> {
        self.transcript().leaf()
    }

    /// Returns to an earlier entry, so the next turn explores a new path from
    /// there.
    ///
    /// The abandoned entries stay in the transcript, reachable through
    /// [`children`](Self::children) — nothing is deleted, so the path just
    /// left can be returned to the same way. Returns how many entries left
    /// the active path.
    pub fn branch_from(
        &mut self,
        entry: &crate::transcript::EntryId,
    ) -> Result<usize, RuntimeError> {
        self.memory.branch_from(entry)
    }

    /// The entries recorded as continuing from `entry`. More than one means
    /// the conversation branched there.
    pub fn children(
        &self,
        entry: &crate::transcript::EntryId,
    ) -> Vec<&crate::transcript::TranscriptItem> {
        self.transcript().children(entry)
    }

    fn append_transcript_item(&mut self, item: TranscriptItem) -> Result<(), RuntimeError> {
        self.memory.append_transcript_item(item)
    }

    pub(crate) fn record_canonical_context(
        &mut self,
        content: impl Into<String>,
    ) -> Result<(), RuntimeError> {
        self.append_transcript_item(TranscriptItem::canonical_context(Message::user(
            ContentBlock::text(content.into()),
        )))
    }

    pub(crate) fn record_delegation_request(
        &mut self,
        content: impl Into<String>,
        delegation: DelegationArtifact,
        edge: Option<DelegationEdge>,
    ) -> Result<(), RuntimeError> {
        self.append_transcript_item(TranscriptItem::delegation_request(
            Message::user(ContentBlock::text(content.into())),
            delegation,
            edge,
        ))
    }

    pub(crate) fn record_delegation_result(
        &mut self,
        content: impl Into<String>,
        delegation: DelegationArtifact,
        edge: Option<DelegationEdge>,
    ) -> Result<(), RuntimeError> {
        self.append_transcript_item(TranscriptItem::delegation_result(
            Message::user(ContentBlock::text(content.into())),
            delegation,
            edge,
        ))
    }

    pub(crate) fn memory_revision(&self) -> u64 {
        self.memory.revision()
    }

    pub(crate) fn memory_engine(&self) -> Arc<crate::memory::MemoryEngine> {
        self.runtime.memory_engine()
    }

    /// Returns whether this agent is a persistent teammate rather than the lead agent.
    pub fn is_teammate(&self) -> bool {
        self.teammate_identity.is_some()
    }

    pub(crate) fn tasks(&self) -> &[TaskItem] {
        &self.tasks
    }

    /// Returns the most recent committed message, if any.
    pub fn last_message(&self) -> Option<&Message> {
        self.memory.last_message()
    }

    /// Subscribes to the agent's transient event stream.
    pub fn subscribe_events(&self) -> broadcast::Receiver<AgentEvent> {
        self.event_bus.subscribe()
    }

    /// Watches the current agent snapshot for state updates.
    pub fn watch_snapshot(&self) -> watch::Receiver<AgentSnapshot> {
        self.snapshot_tx.subscribe()
    }

    /// The tools this agent offers the model on the next round.
    ///
    /// A shaping typed turn (see [`Agent::run_to_output`]) narrows this to
    /// exactly one tool — the terminal tool it generated — because the whole
    /// point of that turn is that the model has nothing to decide but the
    /// answer's shape. A working typed turn narrows nothing: its terminal tool
    /// is admitted by [`can_use_tool`](Self::can_use_tool) like any other, so
    /// it simply joins the ordinary roster.
    pub(crate) fn tools(&self) -> Arc<[crate::tool::ProviderToolSpec]> {
        let gate = self
            .terminal_tool_gate
            .lock()
            .expect("terminal tool gate poisoned")
            .clone();
        self.runtime
            .visible_tool_registrations(&self.id)
            .into_iter()
            .filter(|registration| match &gate {
                Some(gate) if !gate.keeps_tools => {
                    gate.registration.is_same_registration(registration)
                }
                _ => self.registration_is_allowed(registration, gate.as_ref()),
            })
            .map(|registration| registration.descriptor().provider.clone())
            .collect::<Vec<_>>()
            .into()
    }

    pub(crate) fn can_use_tool(&self, name: &str) -> bool {
        matches!(
            self.resolve_tool(name),
            crate::tool::ToolResolution::Visible(_) | crate::tool::ToolResolution::Missing
        )
    }

    pub(crate) fn resolve_tool(&self, name: &str) -> crate::tool::ToolResolution {
        match self.runtime.resolve_tool_for_agent(name, &self.id) {
            crate::tool::ToolResolution::Visible(tool) => {
                let gate = self
                    .terminal_tool_gate
                    .lock()
                    .expect("terminal tool gate poisoned")
                    .clone();
                if self.registration_is_allowed(&tool.registration, gate.as_ref()) {
                    crate::tool::ToolResolution::Visible(tool)
                } else {
                    crate::tool::ToolResolution::Hidden
                }
            }
            crate::tool::ToolResolution::Missing if self.unregistered_name_is_allowed(name) => {
                crate::tool::ToolResolution::Missing
            }
            crate::tool::ToolResolution::Hidden | crate::tool::ToolResolution::Missing => {
                crate::tool::ToolResolution::Hidden
            }
        }
    }

    fn registration_is_allowed(
        &self,
        registration: &crate::tool::ToolRegistration,
        gate: Option<&TerminalToolGate>,
    ) -> bool {
        if let Some(gate) = gate {
            if gate.registration.is_same_registration(registration) {
                return true;
            }
            if gate.registration.name() == registration.name() {
                return false;
            }
        }

        self.name_is_allowed(registration.name(), gate)
    }

    fn unregistered_name_is_allowed(&self, name: &str) -> bool {
        let gate = self
            .terminal_tool_gate
            .lock()
            .expect("terminal tool gate poisoned")
            .clone();
        self.name_is_allowed(name, gate.as_ref())
    }

    fn name_is_allowed(&self, name: &str, gate: Option<&TerminalToolGate>) -> bool {
        if gate.is_some_and(|gate| gate.registration.name() == name) {
            return false;
        }

        if self.hidden_tools.contains(name) {
            return false;
        }

        if !self.config.tool_profile.allows(name) {
            return false;
        }

        if name == RuntimeIntrinsicTool::Idle.to_string() {
            return self.teammate_identity.is_some();
        }

        // The pager's reader is registered in this exact agent's namespace;
        // this config check keeps name-level policy aligned with that lifetime.
        if name == crate::tool::paging::READ_TOOL_RESULT_TOOL {
            return self.config.tool_result_paging.is_some();
        }

        true
    }

    pub(crate) fn runtime_handle(&self) -> RuntimeHandle {
        self.runtime.clone()
    }

    pub(crate) fn replace_tool_authorizer(
        &mut self,
        tool_authorizer: Arc<dyn crate::tool::ToolAuthorizer>,
    ) {
        self.runtime.execution.tool_authorizer = Some(tool_authorizer);
    }

    /// Registers this paging agent's exact reader for its live lifetime.
    fn register_tool_result_pager(&mut self) {
        if self.config.tool_result_paging.is_some() {
            self._tool_result_reader = Some(
                self.runtime
                    .register_agent_tool(&self.id, crate::tool::ReadToolResultTool),
            );
        }
    }

    /// Retains the full text of a result that entered the transcript paged,
    /// so `read_tool_result` can serve its later windows. In memory only, for
    /// this agent's lifetime — a result the model never asks to continue
    /// simply goes away with the agent.
    pub(crate) fn record_paged_tool_result(&self, tool_use_id: &str, full: &str) {
        self.paged_tool_results.record(tool_use_id, full);
    }

    /// Returns a retained full result by `tool_use_id`. Only this agent's own
    /// paged results are reachable: the store is per-agent, so one agent can
    /// never read another's.
    pub(crate) fn paged_tool_result(&self, tool_use_id: &str) -> Option<Arc<str>> {
        self.paged_tool_results.get(tool_use_id)
    }

    pub(crate) fn max_rounds(&self) -> Option<usize> {
        self.max_rounds
    }

    /// What the model is told about choosing a tool on the next round.
    ///
    /// A shaping typed turn forces its terminal tool: it is the only tool on
    /// the request, and the turn exists to produce that one call. A working
    /// typed turn forces nothing — not the terminal tool, which would end the
    /// turn before any work happened, and not the agent's own configured
    /// choice, which would keep the turn from ever reaching the call that ends
    /// it. Both would defeat the mode, so while one runs the choice is `Auto`.
    pub(crate) fn tool_choice(&self) -> Option<ToolChoice> {
        let gate = self
            .terminal_tool_gate
            .lock()
            .expect("terminal tool gate poisoned")
            .clone();
        if let Some(gate) = gate {
            let active = matches!(
                self.runtime
                    .resolve_tool_for_agent(gate.registration.name(), &self.id),
                crate::tool::ToolResolution::Visible(tool)
                    if tool
                        .registration
                        .is_same_registration(&gate.registration)
            );
            if !active {
                return Some(ToolChoice::Auto);
            }
            return Some(if gate.keeps_tools {
                ToolChoice::Auto
            } else {
                ToolChoice::Tool {
                    name: gate.registration.name().to_string(),
                }
            });
        }

        match self.config.tool_choice.clone() {
            Some(ToolChoice::Tool { name }) if !self.can_use_tool(&name) => Some(ToolChoice::Auto),
            other => other,
        }
    }
}
