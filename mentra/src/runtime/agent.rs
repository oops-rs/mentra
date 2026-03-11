mod events;
mod pending;
mod pending_block;
mod runner;
#[cfg(test)]
mod tests;

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use tokio::sync::{broadcast, watch};

use crate::{
    provider::{
        Provider,
        model::{ContentBlock, Message, Role, ToolChoice},
    },
    runtime::{
        TASK_TOOL_NAME, TodoItem,
        error::RuntimeError,
        handle::RuntimeHandle,
        task::{SUBAGENT_MAX_ROUNDS, build_subagent_system_prompt},
        todo::{TODO_REMINDER_TEXT, TODO_REMINDER_THRESHOLD, has_unfinished_todos},
    },
};

pub use events::{
    AgentEvent, AgentSnapshot, AgentStatus, PendingToolUseSummary, SpawnedAgentStatus,
    SpawnedAgentSummary,
};
pub use pending::PendingAssistantTurn;
use runner::TurnRunner;

static NEXT_AGENT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub system: Option<String>,
    pub tool_choice: Option<ToolChoice>,
    pub temperature: Option<f32>,
    pub max_output_tokens: Option<u32>,
    pub metadata: BTreeMap<String, String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            system: None,
            tool_choice: Some(ToolChoice::default()),
            temperature: None,
            max_output_tokens: Some(8192),
            metadata: BTreeMap::new(),
        }
    }
}

pub struct Agent {
    id: String,
    runtime: RuntimeHandle,
    model: String,
    name: String,
    config: AgentConfig,
    history: Vec<Message>,
    todos: Vec<TodoItem>,
    rounds_since_todo: usize,
    event_tx: broadcast::Sender<AgentEvent>,
    snapshot: AgentSnapshot,
    snapshot_tx: watch::Sender<AgentSnapshot>,
    provider: Arc<dyn Provider>,
    hidden_tools: HashSet<String>,
    max_rounds: Option<usize>,
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
    ) -> Self {
        let (event_tx, _) = broadcast::channel(256);
        let snapshot = AgentSnapshot::default();
        let (snapshot_tx, _) = watch::channel(snapshot.clone());

        Self {
            id: format!("agent-{}", NEXT_AGENT_ID.fetch_add(1, Ordering::Relaxed)),
            runtime,
            model,
            name,
            config,
            history: Vec::new(),
            todos: Vec::new(),
            rounds_since_todo: 0,
            event_tx,
            snapshot,
            snapshot_tx,
            provider,
            hidden_tools,
            max_rounds,
        }
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
        self.runtime.tools_excluding(&self.hidden_tools)
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

    pub(crate) fn spawn_subagent(&self) -> Self {
        let mut hidden_tools = self.hidden_tools.clone();
        hidden_tools.insert(TASK_TOOL_NAME.to_string());

        let mut config = self.config.clone();
        config.system = Some(build_subagent_system_prompt(self.effective_system_prompt()));

        Self::new(
            self.runtime.clone(),
            self.model.clone(),
            format!("{}::task", self.name),
            config,
            Arc::clone(&self.provider),
            hidden_tools,
            Some(SUBAGENT_MAX_ROUNDS),
        )
    }

    pub(crate) fn register_subagent(&mut self, agent: &Agent) -> SpawnedAgentSummary {
        let summary = SpawnedAgentSummary {
            id: agent.id.clone(),
            name: agent.name.clone(),
            model: agent.model.clone(),
            status: SpawnedAgentStatus::Running,
        };
        self.snapshot.subagents.push(summary.clone());
        self.publish_snapshot();
        summary
    }

    pub(crate) fn finish_subagent(
        &mut self,
        id: &str,
        status: SpawnedAgentStatus,
    ) -> Option<SpawnedAgentSummary> {
        let summary = self
            .snapshot
            .subagents
            .iter_mut()
            .find(|agent| agent.id == id)?;
        summary.status = status;
        let summary = summary.clone();
        self.publish_snapshot();
        Some(summary)
    }

    pub(crate) fn final_text_summary(&self) -> String {
        let Some(message) = self.last_message() else {
            return "(no summary)".to_string();
        };

        if message.role != Role::Assistant {
            return "(no summary)".to_string();
        }

        let text = message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");

        if text.is_empty() {
            "(no summary)".to_string()
        } else {
            text
        }
    }

    pub async fn send(
        &mut self,
        content: impl Into<Vec<ContentBlock>>,
    ) -> Result<(), RuntimeError> {
        let history_len_before_run = self.history.len();
        let todos_before_run = self.todos.clone();
        let rounds_before_run = self.rounds_since_todo;
        self.push_history(Message {
            role: Role::User,
            content: content.into(),
        });
        self.emit_event(AgentEvent::RunStarted);

        match TurnRunner::new(self).run().await {
            Ok(()) => {
                self.clear_pending_turn();
                self.set_status(AgentStatus::Finished);
                self.emit_event(AgentEvent::RunFinished);
                Ok(())
            }
            Err(error) => {
                self.rollback_history(history_len_before_run);
                self.restore_todo_state(todos_before_run, rounds_before_run);
                self.clear_pending_turn();
                let message = format!("{error:?}");
                self.set_status(AgentStatus::Failed(message.clone()));
                self.emit_event(AgentEvent::RunFailed { error: message });
                Err(error)
            }
        }
    }

    pub(crate) fn push_history(&mut self, message: Message) {
        self.history.push(message);
        self.sync_history_len();
    }

    pub(crate) fn clear_pending_turn(&mut self) {
        self.snapshot.current_text.clear();
        self.snapshot.pending_tool_uses.clear();
        self.publish_snapshot();
    }

    pub(crate) fn publish_pending_turn(&mut self, pending: &PendingAssistantTurn) {
        self.snapshot.current_text = pending.current_text().to_string();
        self.snapshot.pending_tool_uses = pending.pending_tool_use_summaries();
        self.publish_snapshot();
    }

    fn rollback_history(&mut self, history_len: usize) {
        self.history.truncate(history_len);
        self.sync_history_len();
    }

    pub(crate) fn effective_system_prompt(&self) -> Option<Cow<'_, str>> {
        if self.rounds_since_todo < TODO_REMINDER_THRESHOLD || !has_unfinished_todos(&self.todos) {
            return self.config.system.as_deref().map(Cow::Borrowed);
        }

        Some(match &self.config.system {
            Some(system) => Cow::Owned(format!("{TODO_REMINDER_TEXT}\n\n{system}")),
            None => Cow::Borrowed(TODO_REMINDER_TEXT),
        })
    }

    pub(crate) fn apply_todo_items(&mut self, items: Vec<TodoItem>) {
        self.todos = items;
        self.rounds_since_todo = 0;
        self.snapshot.todos = self.todos.clone();
        self.publish_snapshot();
    }

    pub(crate) fn note_round_without_todo(&mut self) {
        if has_unfinished_todos(&self.todos) {
            self.rounds_since_todo += 1;
        }
    }

    fn restore_todo_state(&mut self, todos: Vec<TodoItem>, rounds_since_todo: usize) {
        self.todos = todos;
        self.rounds_since_todo = rounds_since_todo;
        self.snapshot.todos = self.todos.clone();
        self.publish_snapshot();
    }

    pub(crate) fn emit_event(&self, event: AgentEvent) {
        let _ = self.event_tx.send(event);
    }

    pub(crate) fn set_status(&mut self, status: AgentStatus) {
        self.snapshot.status = status;
        self.publish_snapshot();
    }

    fn sync_history_len(&mut self) {
        self.snapshot.history_len = self.history.len();
        self.publish_snapshot();
    }

    fn publish_snapshot(&self) {
        self.snapshot_tx.send_replace(self.snapshot.clone());
    }
}
