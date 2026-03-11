mod compact;
mod config;
mod events;
mod lifecycle;
mod pending;
mod pending_block;
mod runner;
mod snapshot;
mod subagent;
mod team;
mod task_state;
#[cfg(test)]
mod tests;

use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use tokio::sync::{broadcast, watch};

use crate::{
    provider::{Provider, ToolChoice},
    Message,
    runtime::{
        TaskItem,
        background::BackgroundNotification,
        error::RuntimeError,
        handle::RuntimeHandle,
        team::TeamMessage,
    },
};

pub use config::{AgentConfig, ContextCompactionConfig, TaskGraphConfig, TeamConfig};
pub use events::{
    AgentEvent, AgentSnapshot, AgentStatus, ContextCompactionDetails, ContextCompactionTrigger,
    PendingToolUseSummary, SpawnedAgentStatus, SpawnedAgentSummary,
};
pub use pending::PendingAssistantTurn;
use runner::TurnRunner;

static NEXT_AGENT_ID: AtomicU64 = AtomicU64::new(1);

pub struct Agent {
    id: String,
    runtime: RuntimeHandle,
    model: String,
    name: String,
    config: AgentConfig,
    history: Vec<Message>,
    tasks: Vec<TaskItem>,
    rounds_since_task_graph: usize,
    event_tx: broadcast::Sender<AgentEvent>,
    snapshot: Arc<Mutex<AgentSnapshot>>,
    snapshot_tx: watch::Sender<AgentSnapshot>,
    provider: Arc<dyn Provider>,
    hidden_tools: HashSet<String>,
    max_rounds: Option<usize>,
    inflight_background_notifications: Vec<BackgroundNotification>,
    inflight_team_messages: Vec<TeamMessage>,
}

impl Agent {
    pub(crate) fn new(
        runtime: RuntimeHandle,
        model: String,
        name: String,
        config: AgentConfig,
        provider: Arc<dyn Provider>,
        hidden_tools: HashSet<String>,
        max_rounds: Option<usize>,
    ) -> Result<Self, RuntimeError> {
        let (event_tx, _) = broadcast::channel(256);
        let snapshot = AgentSnapshot::default();
        let snapshot = Arc::new(Mutex::new(snapshot));
        let (snapshot_tx, _) =
            watch::channel(snapshot.lock().expect("agent snapshot poisoned").clone());
        let mut agent = Self {
            id: format!("agent-{}", NEXT_AGENT_ID.fetch_add(1, Ordering::Relaxed)),
            runtime,
            model,
            name,
            config,
            history: Vec::new(),
            tasks: Vec::new(),
            rounds_since_task_graph: 0,
            event_tx,
            snapshot,
            snapshot_tx,
            provider,
            hidden_tools,
            max_rounds,
            inflight_background_notifications: Vec::new(),
            inflight_team_messages: Vec::new(),
        };
        agent.runtime.register_agent(
            &agent.id,
            &agent.name,
            agent.config.team.team_dir.as_path(),
            agent.event_tx.clone(),
            agent.snapshot_tx.clone(),
            Arc::clone(&agent.snapshot),
        )?;
        agent.refresh_tasks_from_disk()?;
        Ok(agent)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    pub fn history(&self) -> &[Message] {
        &self.history
    }

    pub fn last_message(&self) -> Option<&Message> {
        self.history.last()
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<AgentEvent> {
        self.event_tx.subscribe()
    }

    pub fn watch_snapshot(&self) -> watch::Receiver<AgentSnapshot> {
        self.snapshot_tx.subscribe()
    }

    pub(crate) fn tools(&self) -> Arc<[crate::tool::ToolSpec]> {
        let mut tools = if self.runtime.runtime_intrinsics_enabled() {
            crate::runtime::intrinsic::specs()
        } else {
            Vec::new()
        };
        tools.extend(
            self.runtime
                .tools()
                .iter()
                .filter(|tool| !self.hidden_tools.contains(&tool.name))
                .cloned(),
        );
        tools.retain(|tool| !self.hidden_tools.contains(&tool.name));
        tools.into()
    }

    pub(crate) fn can_use_tool(&self, name: &str) -> bool {
        !self.hidden_tools.contains(name)
    }

    pub(crate) fn max_rounds(&self) -> Option<usize> {
        self.max_rounds
    }

    pub(crate) fn tool_choice(&self) -> Option<ToolChoice> {
        match self.config.tool_choice.clone() {
            Some(ToolChoice::Tool { name }) if self.hidden_tools.contains(&name) => {
                Some(ToolChoice::Auto)
            }
            other => other,
        }
    }
}
