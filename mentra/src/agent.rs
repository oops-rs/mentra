mod compact;
mod config;
mod events;
mod lifecycle;
mod pending;
mod pending_block;
mod runner;
mod snapshot;
mod subagent;
mod task_state;
mod team;
#[cfg(test)]
mod tests;

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
    AgentConfig, CompactionConfig, ContextCompactionConfig, MemoryConfig, TaskConfig,
    TeamAutonomyConfig, TeamConfig, ToolProfile, WorkspaceConfig,
};
pub use events::{
    AgentEvent, AgentSnapshot, AgentStatus, CompactionDetails, CompactionTrigger,
    ContextCompactionDetails, ContextCompactionTrigger, PendingToolUseSummary, SpawnedAgentStatus,
    SpawnedAgentSummary,
};
pub use pending::PendingAssistantTurn;
use runner::TurnRunner;
pub(crate) use subagent::DisposableSubagentTemplate;

static NEXT_AGENT_ID: AtomicU64 = AtomicU64::new(1);

/// Running or persisted agent managed by a [`crate::Runtime`].
pub struct Agent {
    id: String,
    runtime: RuntimeHandle,
    model: String,
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
    max_rounds: Option<usize>,
    inflight_background_notifications: Vec<BackgroundNotification>,
    inflight_team_messages: Vec<TeamMessage>,
    teammate_identity: Option<TeammateIdentity>,
    idle_requested: bool,
    current_run_id: Option<String>,
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

pub(crate) struct AgentEventTapGuard {
    registry: Arc<Mutex<AgentEventTapRegistry>>,
    id: u64,
}

#[derive(Clone)]
pub(crate) struct AgentEventBus {
    tx: broadcast::Sender<AgentEvent>,
    taps: Arc<Mutex<AgentEventTapRegistry>>,
}

impl AgentEventBus {
    fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            tx,
            taps: Arc::new(Mutex::new(AgentEventTapRegistry::default())),
        }
    }

    pub(crate) fn send(&self, event: AgentEvent) {
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
            id,
        }
    }
}

impl Drop for AgentEventTapGuard {
    fn drop(&mut self) {
        let mut registry = self
            .registry
            .lock()
            .expect("agent event tap registry poisoned");
        registry.taps.retain(|(tap_id, _)| *tap_id != self.id);
    }
}

impl Agent {
    pub(crate) fn new(
        runtime: RuntimeHandle,
        model: String,
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
            max_rounds,
            inflight_background_notifications: Vec::new(),
            inflight_team_messages: Vec::new(),
            teammate_identity,
            idle_requested: false,
            current_run_id: None,
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
        agent.refresh_tasks_from_disk()?;
        Ok(agent)
    }

    pub(crate) fn from_loaded(
        runtime: RuntimeHandle,
        mut state: LoadedAgentState,
        provider: Arc<dyn Provider>,
    ) -> Result<Self, RuntimeError> {
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
            max_rounds: state.record.max_rounds,
            inflight_background_notifications: Vec::new(),
            inflight_team_messages: Vec::new(),
            teammate_identity: state.record.teammate_identity,
            idle_requested: state.record.idle_requested,
            current_run_id: None,
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

    /// Updates the model and provider used for future turns, then persists the
    /// new agent record so resumed sessions continue with the same setting.
    pub fn set_model(&mut self, model: crate::ModelInfo) -> Result<(), RuntimeError> {
        let provider = self
            .runtime
            .get_provider(Some(&model.provider))
            .ok_or_else(|| RuntimeError::ProviderNotFound(Some(model.provider.clone())))?;
        self.model = model.id;
        self.provider_id = provider.descriptor().id;
        self.provider = provider;
        self.persist_agent_record()
    }

    /// Returns the effective agent configuration.
    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    /// Returns the committed transcript history.
    pub fn history(&self) -> &[Message] {
        self.memory.history()
    }

    /// Returns the canonical transcript items stored for this agent.
    pub fn transcript(&self) -> &crate::AgentTranscript {
        self.memory.transcript()
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

    pub(crate) fn tools(&self) -> Arc<[crate::tool::ProviderToolSpec]> {
        self.runtime
            .tools()
            .iter()
            .filter(|tool| self.can_use_tool(&tool.name))
            .cloned()
            .collect::<Vec<_>>()
            .into()
    }

    pub(crate) fn can_use_tool(&self, name: &str) -> bool {
        if self.hidden_tools.contains(name) {
            return false;
        }

        if !self.config.tool_profile.allows(name) {
            return false;
        }

        if name == RuntimeIntrinsicTool::Idle.to_string() {
            return self.teammate_identity.is_some();
        }

        true
    }

    pub(crate) fn runtime_handle(&self) -> RuntimeHandle {
        self.runtime.clone()
    }

    pub(crate) fn max_rounds(&self) -> Option<usize> {
        self.max_rounds
    }

    pub(crate) fn tool_choice(&self) -> Option<ToolChoice> {
        match self.config.tool_choice.clone() {
            Some(ToolChoice::Tool { name }) if !self.can_use_tool(&name) => Some(ToolChoice::Auto),
            other => other,
        }
    }
}
