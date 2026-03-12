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
    Message,
    provider::{Provider, ProviderId, ToolChoice},
    runtime::{
        LoadedAgentState, RuntimeIntrinsicTool, TaskItem,
        background::BackgroundNotification,
        error::RuntimeError,
        handle::{AgentExecutionConfig, AgentObserver, RuntimeHandle},
        team::TeamMessage,
    },
};

pub use config::{
    AgentConfig, ContextCompactionConfig, TaskConfig, TeamAutonomyConfig, TeamConfig,
    WorkspaceConfig,
};
pub use events::{
    AgentEvent, AgentSnapshot, AgentStatus, ContextCompactionDetails, ContextCompactionTrigger,
    PendingToolUseSummary, SpawnedAgentStatus, SpawnedAgentSummary,
};
pub use pending::PendingAssistantTurn;
use runner::TurnRunner;
pub(crate) use team::parse_task_input;

static NEXT_AGENT_ID: AtomicU64 = AtomicU64::new(1);

/// Running or persisted agent managed by a [`crate::runtime::Runtime`].
pub struct Agent {
    id: String,
    runtime: RuntimeHandle,
    model: String,
    provider_id: ProviderId,
    name: String,
    config: AgentConfig,
    history: Vec<Message>,
    tasks: Vec<TaskItem>,
    rounds_since_task: usize,
    event_tx: broadcast::Sender<AgentEvent>,
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
        let (event_tx, _) = broadcast::channel(256);
        let snapshot = AgentSnapshot::default();
        let snapshot = Arc::new(Mutex::new(snapshot));
        let (snapshot_tx, _) =
            watch::channel(snapshot.lock().expect("agent snapshot poisoned").clone());
        let mut agent = Self {
            id: format!(
                "agent-{:x}-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
                NEXT_AGENT_ID.fetch_add(1, Ordering::Relaxed)
            ),
            runtime,
            model,
            provider_id: provider.descriptor().id,
            name,
            config,
            history: Vec::new(),
            tasks: Vec::new(),
            rounds_since_task: 0,
            event_tx,
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
        agent.persist_state()?;
        let execution_config = AgentExecutionConfig {
            name: agent.name.clone(),
            team_dir: agent.config.team.team_dir.clone(),
            tasks_dir: agent.config.task.tasks_dir.clone(),
            base_dir: agent.config.workspace.base_dir.clone(),
            auto_route_shell: agent.config.workspace.auto_route_shell,
            is_teammate: agent.teammate_identity.is_some(),
        };
        let observer = AgentObserver {
            events: agent.event_tx.clone(),
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
        state: LoadedAgentState,
        provider: Arc<dyn Provider>,
    ) -> Result<Self, RuntimeError> {
        let snapshot = AgentSnapshot {
            status: state.record.status.clone(),
            history_len: state.history.len(),
            current_text: state
                .pending_turn
                .as_ref()
                .map(|pending| pending.current_text.clone())
                .unwrap_or_default(),
            pending_tool_uses: state
                .pending_turn
                .as_ref()
                .map(|pending| pending.pending_tool_uses.clone())
                .unwrap_or_default(),
            pending_team_messages: 0,
            tasks: Vec::new(),
            subagents: state.record.subagents.clone(),
            teammates: Vec::new(),
            protocol_requests: Vec::new(),
            background_tasks: Vec::new(),
        };
        let snapshot = Arc::new(Mutex::new(snapshot));
        let (snapshot_tx, _) =
            watch::channel(snapshot.lock().expect("agent snapshot poisoned").clone());
        let (event_tx, _) = broadcast::channel(256);
        let mut agent = Self {
            id: state.record.id.clone(),
            runtime,
            model: state.record.model.clone(),
            provider_id: state.record.provider_id.clone(),
            name: state.record.name.clone(),
            config: state.record.config.clone(),
            history: state.history,
            tasks: Vec::new(),
            rounds_since_task: state.record.rounds_since_task,
            event_tx,
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
            auto_route_shell: agent.config.workspace.auto_route_shell,
            is_teammate: agent.teammate_identity.is_some(),
        };
        let observer = AgentObserver {
            events: agent.event_tx.clone(),
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

    /// Returns the effective agent configuration.
    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    /// Returns the committed transcript history.
    pub fn history(&self) -> &[Message] {
        &self.history
    }

    pub(crate) fn tasks(&self) -> &[TaskItem] {
        &self.tasks
    }

    /// Returns the most recent committed message, if any.
    pub fn last_message(&self) -> Option<&Message> {
        self.history.last()
    }

    /// Subscribes to the agent's transient event stream.
    pub fn subscribe_events(&self) -> broadcast::Receiver<AgentEvent> {
        self.event_tx.subscribe()
    }

    /// Watches the current agent snapshot for state updates.
    pub fn watch_snapshot(&self) -> watch::Receiver<AgentSnapshot> {
        self.snapshot_tx.subscribe()
    }

    pub(crate) fn tools(&self) -> Arc<[crate::tool::ToolSpec]> {
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

        if name == RuntimeIntrinsicTool::Idle.to_string() {
            return self.teammate_identity.is_some();
        }

        true
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
