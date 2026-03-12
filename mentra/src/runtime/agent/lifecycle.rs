use crate::{
    ContentBlock, Message, Role,
    runtime::{RunOptions, error::RuntimeError},
};

use super::{Agent, AgentEvent, AgentStatus, TurnRunner};

impl Agent {
    /// Sends a user turn using default run options.
    pub async fn send(
        &mut self,
        content: impl Into<Vec<ContentBlock>>,
    ) -> Result<(), RuntimeError> {
        self.run(content, RunOptions::default()).await
    }

    /// Runs a user turn with explicit execution limits and cancellation settings.
    pub async fn run(
        &mut self,
        content: impl Into<Vec<ContentBlock>>,
        options: RunOptions,
    ) -> Result<(), RuntimeError> {
        self.idle_requested = false;
        self.refresh_tasks_from_disk()?;
        let history_before_run = self.history.clone();
        let tasks_before_run = self.tasks.clone();
        let rounds_before_run = self.rounds_since_task;
        let task_disk_state = self.capture_task_disk_state()?;
        self.start_run_checkpoint()?;
        self.push_history(Message {
            role: Role::User,
            content: content.into(),
        });
        self.persist_state()?;
        self.emit_event(AgentEvent::RunStarted);

        match TurnRunner::new(self, options).run().await {
            Ok(()) => {
                self.clear_inflight_team_messages();
                self.clear_inflight_background_notifications();
                self.clear_pending_turn();
                self.clear_persisted_pending_turn()?;
                self.set_status(AgentStatus::Finished);
                self.persist_state()?;
                self.finish_run_checkpoint()?;
                self.emit_event(AgentEvent::RunFinished);
                Ok(())
            }
            Err(error) => {
                self.idle_requested = false;
                self.requeue_inflight_team_messages()?;
                self.requeue_inflight_background_notifications();
                self.restore_history(history_before_run);
                self.restore_task_state(tasks_before_run, rounds_before_run, &task_disk_state)?;
                self.clear_pending_turn();
                self.clear_persisted_pending_turn()?;
                let message = error.to_string();
                self.set_status(AgentStatus::Failed(message.clone()));
                self.persist_state()?;
                self.fail_run_checkpoint(&message)?;
                self.emit_event(AgentEvent::RunFailed { error: message });
                Err(error)
            }
        }
    }

    pub(crate) fn request_idle(&mut self) {
        self.idle_requested = true;
    }

    pub(crate) fn take_idle_requested(&mut self) -> bool {
        std::mem::take(&mut self.idle_requested)
    }
}
