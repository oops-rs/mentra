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

use tokio::sync::{broadcast, watch};

use crate::{
    Message,
    provider::{Provider, ToolChoice},
    runtime::{
        TaskItem, background::BackgroundNotification, error::RuntimeError,
        handle::{AgentExecutionConfig, AgentObserver, RuntimeHandle},
        team::TeamMessage,
    },
};

pub use config::{
    AgentConfig, ContextCompactionConfig, ExecutionContextConfig, TaskConfig, TeamAutonomyConfig,
    TeamConfig,
};
pub use events::{
    AgentEvent, AgentSnapshot, AgentStatus, ContextCompactionDetails, ContextCompactionTrigger,
    PendingToolUseSummary, SpawnedAgentStatus, SpawnedAgentSummary,
};
pub use pending::PendingAssistantTurn;
use runner::TurnRunner;
pub(crate) use team::parse_task_input;

static NEXT_AGENT_ID: AtomicU64 = AtomicU64::new(1);

pub struct Agent {
    id: String,
    runtime: RuntimeHandle,
    model: String,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
            id: format!("agent-{}", NEXT_AGENT_ID.fetch_add(1, Ordering::Relaxed)),
            runtime,
            model,
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
        };
        let execution_config = AgentExecutionConfig {
            name: agent.name.clone(),
            team_dir: agent.config.team.team_dir.clone(),
            tasks_dir: agent.config.task.tasks_dir.clone(),
            base_dir: agent.config.execution_context.base_dir.clone(),
            contexts_dir: agent.config.execution_context.contexts_dir.clone(),
            auto_route_shell: agent.config.execution_context.auto_route_shell,
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
        agent.refresh_execution_contexts_from_disk()?;
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
                .filter(|tool| self.can_use_tool(&tool.name))
                .cloned(),
        );
        tools.retain(|tool| self.can_use_tool(&tool.name));
        tools.into()
    }

    pub(crate) fn can_use_tool(&self, name: &str) -> bool {
        if self.hidden_tools.contains(name) {
            return false;
        }

        if name == crate::runtime::intrinsic::IDLE_TOOL_NAME {
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
