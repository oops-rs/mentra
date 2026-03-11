use crate::{
    ContentBlock, Message, Role,
    runtime::error::RuntimeError,
};

use super::{Agent, AgentEvent, AgentStatus, TurnRunner};

impl Agent {
    pub async fn send(
        &mut self,
        content: impl Into<Vec<ContentBlock>>,
    ) -> Result<(), RuntimeError> {
        self.refresh_tasks_from_disk()?;
        let history_before_run = self.history.clone();
        let tasks_before_run = self.tasks.clone();
        let rounds_before_run = self.rounds_since_task_graph;
        let task_disk_state = self.capture_task_disk_state()?;
        self.push_history(Message {
            role: Role::User,
            content: content.into(),
        });
        self.emit_event(AgentEvent::RunStarted);

        match TurnRunner::new(self).run().await {
            Ok(()) => {
                self.clear_inflight_team_messages();
                self.clear_inflight_background_notifications();
                self.clear_pending_turn();
                self.set_status(AgentStatus::Finished);
                self.emit_event(AgentEvent::RunFinished);
                Ok(())
            }
            Err(error) => {
                self.requeue_inflight_team_messages()?;
                self.requeue_inflight_background_notifications();
                self.restore_history(history_before_run);
                self.restore_task_state(tasks_before_run, rounds_before_run, &task_disk_state)?;
                self.clear_pending_turn();
                let message = format!("{error:?}");
                self.set_status(AgentStatus::Failed(message.clone()));
                self.emit_event(AgentEvent::RunFailed { error: message });
                Err(error)
            }
        }
    }
}
