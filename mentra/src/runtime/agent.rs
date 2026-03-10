mod events;
mod pending;
mod pending_block;
mod runner;
#[cfg(test)]
mod tests;

use std::{collections::BTreeMap, sync::Arc};

use tokio::sync::{broadcast, watch};

use crate::{
    provider::{
        Provider,
        model::{ContentBlock, Message, Role, ToolChoice},
    },
    runtime::{
        TodoItem,
        error::RuntimeError,
        handle::RuntimeHandle,
        todo::{TODO_REMINDER_TEXT, TODO_REMINDER_THRESHOLD, has_unfinished_todos},
    },
};

pub use events::{AgentEvent, AgentSnapshot, AgentStatus, PendingToolUseSummary};
pub use pending::PendingAssistantTurn;
use runner::TurnRunner;

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
            tool_choice: Some(ToolChoice::Auto),
            temperature: None,
            max_output_tokens: Some(8192),
            metadata: BTreeMap::new(),
        }
    }
}

pub struct Agent {
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
}

impl Agent {
    pub(crate) fn new(
        runtime: RuntimeHandle,
        model: String,
        name: String,
        config: AgentConfig,
        provider: Arc<dyn Provider>,
    ) -> Self {
        let (event_tx, _) = broadcast::channel(256);
        let snapshot = AgentSnapshot::default();
        let (snapshot_tx, _) = watch::channel(snapshot.clone());

        Self {
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
        }
    }

    pub fn name(&self) -> &str {
        &self.name
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

    pub(crate) fn effective_system_prompt(&self) -> Option<String> {
        if self.rounds_since_todo < TODO_REMINDER_THRESHOLD || !has_unfinished_todos(&self.todos) {
            return self.config.system.clone();
        }

        Some(match &self.config.system {
            Some(system) => format!("{TODO_REMINDER_TEXT}\n\n{system}"),
            None => TODO_REMINDER_TEXT.to_string(),
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
