use crate::{ContentBlock, Message, Role, error::RuntimeError, runtime::RunOptions};

use super::{Agent, AgentEvent, AgentStatus, TurnRunner};

impl Agent {
    /// Sends a user turn using default run options.
    pub async fn send(
        &mut self,
        content: impl Into<Vec<ContentBlock>>,
    ) -> Result<(), RuntimeError> {
        self.run(content, RunOptions::default()).await
    }

    /// Replays the most recent failed or interrupted user turn using default run options.
    pub async fn resume(&mut self) -> Result<(), RuntimeError> {
        self.resume_with_options(RunOptions::default()).await
    }

    /// Replays the most recent failed or interrupted user turn with explicit execution options.
    pub async fn resume_with_options(&mut self, options: RunOptions) -> Result<(), RuntimeError> {
        let content = self
            .memory
            .resumable_user_message()
            .ok_or(RuntimeError::NoResumableTurn)?
            .content
            .clone();
        self.run(content, options).await
    }

    /// Runs a user turn with explicit execution limits and cancellation settings.
    pub async fn run(
        &mut self,
        content: impl Into<Vec<ContentBlock>>,
        options: RunOptions,
    ) -> Result<(), RuntimeError> {
        self.idle_requested = false;
        self.refresh_tasks_from_disk()?;
        let tasks_before_run = self.tasks.clone();
        let rounds_before_run = self.rounds_since_task;
        let task_disk_state = self.capture_task_disk_state()?;
        let run_id = self.start_run_checkpoint()?;
        self.memory.begin_run(
            run_id,
            Message {
                role: Role::User,
                content: content.into(),
            },
        )?;
        self.sync_memory_snapshot();
        self.emit_event(AgentEvent::RunStarted);

        match TurnRunner::new(self, options).run().await {
            Ok(()) => {
                let run_delta = self.memory.current_run_delta().unwrap_or_default();
                self.clear_inflight_team_messages();
                self.clear_inflight_background_notifications();
                self.memory.finish_run()?;
                self.sync_memory_snapshot();
                self.runtime
                    .memory_engine()
                    .schedule_ingest(crate::memory::IngestRequest {
                        agent_id: self.id().to_string(),
                        source_revision: self.memory.revision(),
                        messages: run_delta,
                    });
                self.set_status(AgentStatus::Finished);
                self.persist_agent_record()?;
                self.finish_run_checkpoint()?;
                self.emit_event(AgentEvent::RunFinished);
                Ok(())
            }
            Err(error) => {
                self.idle_requested = false;
                self.requeue_inflight_team_messages()?;
                self.requeue_inflight_background_notifications();
                self.restore_task_state(tasks_before_run, rounds_before_run, &task_disk_state)?;
                self.memory.rollback_failed_run()?;
                self.sync_memory_snapshot();
                let message = error.to_string();
                self.set_status(AgentStatus::Failed(message.clone()));
                self.persist_agent_record()?;
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
