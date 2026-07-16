use crate::{agent::AgentEvent, runtime::PersistedAgentRecord};

use super::{Agent, AgentEventTapGuard, AgentStatus};

impl Agent {
    pub(crate) fn emit_event(&self, event: AgentEvent) {
        self.event_bus.send(event);
    }

    pub(crate) fn event_sender(&self) -> super::AgentEventBus {
        self.event_bus.clone()
    }

    pub(crate) fn register_event_tap(
        &self,
        tap: impl Fn(&AgentEvent) + Send + Sync + 'static,
    ) -> AgentEventTapGuard {
        self.event_bus.register_tap(tap)
    }

    pub(crate) fn set_status(&mut self, status: AgentStatus) {
        self.mutate_snapshot(|snapshot| {
            snapshot.status = status;
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

    pub(super) fn mutate_snapshot(&self, update: impl FnOnce(&mut crate::agent::AgentSnapshot)) {
        {
            let mut snapshot = self.snapshot.lock().expect("agent snapshot poisoned");
            update(&mut snapshot);
        }
        self.publish_snapshot();
    }

    pub(crate) fn sync_memory_snapshot(&self) {
        let memory_view = self.memory.snapshot_view();
        self.mutate_snapshot(|snapshot| {
            snapshot.history_len = memory_view.history_len;
            snapshot.current_text = memory_view.current_text;
            snapshot.pending_tool_uses = memory_view.pending_tool_uses;
        });
    }

    pub(crate) fn persisted_record(&self) -> PersistedAgentRecord {
        PersistedAgentRecord {
            id: self.id.clone(),
            runtime_identifier: self.runtime.persisted_runtime_identifier().to_string(),
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
        }
    }

    pub(crate) fn persist_agent_record(&self) -> Result<(), crate::error::RuntimeError> {
        self.runtime
            .store()
            .save_agent_record(&self.persisted_record())
    }

    pub(crate) fn start_run_checkpoint(&mut self) -> Result<String, crate::runtime::RuntimeError> {
        let run_id = self.runtime.store().start_run(&self.id)?;
        self.current_run_id = Some(run_id.clone());
        Ok(run_id)
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
        if let Some(run_id) = self.current_run_id.as_deref() {
            self.runtime.store().finish_run(run_id)?;
            self.current_run_id = None;
        }
        Ok(())
    }

    pub(crate) fn fail_run_checkpoint(
        &mut self,
        error: &str,
    ) -> Result<(), crate::runtime::RuntimeError> {
        if let Some(run_id) = self.current_run_id.as_deref() {
            self.runtime.store().fail_run(run_id, error)?;
            self.current_run_id = None;
        }
        Ok(())
    }
}
