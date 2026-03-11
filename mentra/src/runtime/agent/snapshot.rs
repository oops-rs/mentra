use crate::{provider::model::Message, runtime::AgentEvent};

use super::{Agent, AgentStatus, PendingAssistantTurn};

impl Agent {
    pub(crate) fn push_history(&mut self, message: Message) {
        self.history.push(message);
        self.sync_history_len();
    }

    pub(crate) fn replace_history(&mut self, history: Vec<Message>) {
        self.history = history;
        self.sync_history_len();
    }

    pub(crate) fn clear_pending_turn(&mut self) {
        self.mutate_snapshot(|snapshot| {
            snapshot.current_text.clear();
            snapshot.pending_tool_uses.clear();
        });
    }

    pub(crate) fn publish_pending_turn(&mut self, pending: &PendingAssistantTurn) {
        self.mutate_snapshot(|snapshot| {
            snapshot.current_text = pending.current_text().to_string();
            snapshot.pending_tool_uses = pending.pending_tool_use_summaries();
        });
    }

    pub(crate) fn emit_event(&self, event: AgentEvent) {
        let _ = self.event_tx.send(event);
    }

    pub(crate) fn set_status(&mut self, status: AgentStatus) {
        self.mutate_snapshot(|snapshot| {
            snapshot.status = status;
        });
    }

    pub(super) fn restore_history(&mut self, history: Vec<Message>) {
        self.history = history;
        self.sync_history_len();
    }

    fn sync_history_len(&mut self) {
        let history_len = self.history.len();
        self.mutate_snapshot(|snapshot| {
            snapshot.history_len = history_len;
        });
    }

    pub(super) fn publish_snapshot(&self) {
        let snapshot = self
            .snapshot
            .lock()
            .expect("agent snapshot poisoned")
            .clone();
        self.snapshot_tx.send_replace(snapshot);
    }

    pub(super) fn mutate_snapshot(&self, update: impl FnOnce(&mut crate::runtime::AgentSnapshot)) {
        {
            let mut snapshot = self.snapshot.lock().expect("agent snapshot poisoned");
            update(&mut snapshot);
        }
        self.publish_snapshot();
    }
}
