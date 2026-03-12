use crate::{Message, runtime::AgentEvent};

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

    pub(crate) fn persist_state(&self) -> Result<(), crate::runtime::RuntimeError> {
        self.runtime.store().save_agent_checkpoint(
            &crate::runtime::PersistedAgentRecord {
                id: self.id.clone(),
                name: self.name.clone(),
                model: self.model.clone(),
                provider_id: self.provider_id.clone(),
                config: self.config.clone(),
                hidden_tools: self.hidden_tools.clone(),
                max_rounds: self.max_rounds,
                teammate_identity: self.teammate_identity.clone(),
                rounds_since_task: self.rounds_since_task,
                idle_requested: self.idle_requested,
                status: self.watch_snapshot().borrow().status.clone(),
                subagents: self.watch_snapshot().borrow().subagents.clone(),
            },
            &self.history,
        )
    }

    pub(crate) fn persist_pending_turn(
        &self,
        pending: &PendingAssistantTurn,
    ) -> Result<(), crate::runtime::RuntimeError> {
        self.runtime.store().save_pending_turn(
            &self.id,
            &crate::runtime::PersistedPendingTurn {
                current_text: pending.current_text().to_string(),
                pending_tool_uses: pending.pending_tool_use_summaries(),
            },
        )
    }

    pub(crate) fn clear_persisted_pending_turn(&self) -> Result<(), crate::runtime::RuntimeError> {
        self.runtime.store().clear_pending_turn(&self.id)
    }

    pub(crate) fn start_run_checkpoint(&mut self) -> Result<(), crate::runtime::RuntimeError> {
        let run_id = self.runtime.store().start_run(&self.id)?;
        self.current_run_id = Some(run_id);
        Ok(())
    }

    pub(crate) fn update_run_state(
        &self,
        state: &str,
        error: Option<&str>,
    ) -> Result<(), crate::runtime::RuntimeError> {
        if let Some(run_id) = &self.current_run_id {
            self.runtime
                .store()
                .update_run_state(run_id, state, error)?;
        }
        Ok(())
    }

    pub(crate) fn finish_run_checkpoint(&mut self) -> Result<(), crate::runtime::RuntimeError> {
        if let Some(run_id) = self.current_run_id.take() {
            self.runtime.store().finish_run(&run_id)?;
        }
        Ok(())
    }

    pub(crate) fn fail_run_checkpoint(
        &mut self,
        error: &str,
    ) -> Result<(), crate::runtime::RuntimeError> {
        if let Some(run_id) = self.current_run_id.take() {
            self.runtime.store().fail_run(&run_id, error)?;
        }
        Ok(())
    }
}
