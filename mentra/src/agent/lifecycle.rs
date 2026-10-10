use crate::{ContentBlock, Message, Role, error::RuntimeError, runtime::RunOptions};

use super::{Agent, AgentEvent, AgentStatus, TurnRunner};

/// How a run opens.
enum RunStart {
    /// With a new user message.
    UserTurn(Message),
    /// From the transcript's end, finishing an interrupted turn.
    Continuation,
}

impl Agent {
    /// Sends a user turn using default run options.
    pub async fn send(
        &mut self,
        content: impl Into<Vec<ContentBlock>>,
    ) -> Result<Message, RuntimeError> {
        self.run(content, RunOptions::default()).await
    }

    /// Replays the most recent failed or interrupted user turn using default run options.
    pub async fn resume(&mut self) -> Result<Message, RuntimeError> {
        self.resume_with_options(RunOptions::default()).await
    }

    /// Whether this agent's transcript ends inside a turn a crash interrupted
    /// after the model had acted on it: tool calls ran, and the model has not
    /// answered since.
    ///
    /// Recovery closed any call that was still running with an error result
    /// saying its effect is unknown, so the transcript is valid as it stands.
    /// Such a turn is picked back up with
    /// [`continue_interrupted`](Self::continue_interrupted);
    /// [`resume`](Self::resume) replays only a turn that committed nothing.
    /// Cleared once any run finishes, or by
    /// [`branch_from`](Self::branch_from).
    pub fn has_interrupted_turn(&self) -> bool {
        self.memory.has_interrupted_turn()
    }

    /// Continues an interrupted turn from the transcript's end, without
    /// appending a user message: the model is asked again with what the
    /// turn already committed, including the closed calls, and the turn
    /// runs on from there exactly as [`run`](Self::run) would — the same
    /// run checkpoint, events, limits, task bookkeeping and rollback.
    ///
    /// No interrupted call is ever re-executed by this; the model decides
    /// what to do about one. A continuation that fails rolls back to the
    /// turn it set out to continue, which stays continuable.
    ///
    /// Fails with [`RuntimeError::NoInterruptedTurn`] when
    /// [`has_interrupted_turn`](Self::has_interrupted_turn) is false.
    pub async fn continue_interrupted(
        &mut self,
        options: RunOptions,
    ) -> Result<Message, RuntimeError> {
        if !self.memory.has_interrupted_turn() {
            return Err(RuntimeError::NoInterruptedTurn);
        }
        self.drive_run(RunStart::Continuation, options).await
    }

    /// Replays the most recent failed or interrupted user turn with explicit
    /// execution options.
    ///
    /// Only a turn that committed nothing the model said is replayable: one
    /// interrupted after its tool calls ran is continued with
    /// [`continue_interrupted`](Self::continue_interrupted) instead, and this
    /// returns [`RuntimeError::NoResumableTurn`] for it.
    pub async fn resume_with_options(
        &mut self,
        options: RunOptions,
    ) -> Result<Message, RuntimeError> {
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
    ) -> Result<Message, RuntimeError> {
        let message = Message {
            role: Role::User,
            content: content.into(),
        };
        self.drive_run(RunStart::UserTurn(message), options).await
    }

    /// The one path every run takes, whether it opens with a user message or
    /// continues an interrupted turn: only how memory opens the run differs.
    async fn drive_run(
        &mut self,
        start: RunStart,
        options: RunOptions,
    ) -> Result<Message, RuntimeError> {
        self.idle_requested = false;
        self.refresh_tasks_from_disk()?;
        let tasks_before_run = self.tasks.clone();
        let rounds_before_run = self.rounds_since_task;
        let task_disk_state = self.capture_task_disk_state()?;
        let run_id = self.start_run_checkpoint()?;
        match start {
            RunStart::UserTurn(message) => self.memory.begin_run(run_id, message)?,
            RunStart::Continuation => self.memory.begin_continuation(run_id)?,
        }
        self.mutate_snapshot(|snapshot| {
            snapshot.run_generation = snapshot.run_generation.saturating_add(1);
            snapshot.status = AgentStatus::AwaitingModel;
        });
        self.sync_memory_snapshot();
        self.emit_event(AgentEvent::RunStarted);

        match TurnRunner::new(self, options).run().await {
            Ok(()) => {
                let final_message = self
                    .memory
                    .last_message()
                    .cloned()
                    .filter(|message| message.role == Role::Assistant);
                let run_delta = self.memory.current_run_delta().unwrap_or_default();
                let finalization = (|| {
                    self.memory.finish_run()?;
                    self.sync_memory_snapshot();
                    self.set_status(AgentStatus::Finished);
                    self.persist_agent_record()?;
                    self.finish_run_checkpoint()
                })();
                if let Err(error) = finalization {
                    return Err(self.handle_finalization_error(error));
                }

                self.clear_inflight_steering();
                self.clear_inflight_team_messages();
                self.clear_inflight_background_notifications();
                self.runtime
                    .memory_engine()
                    .schedule_ingest(crate::memory::IngestRequest {
                        agent_id: self.id().to_string(),
                        source_revision: self.memory.revision(),
                        messages: run_delta,
                    });
                self.emit_event(AgentEvent::RunFinished);
                final_message.ok_or(RuntimeError::EmptyAssistantResponse)
            }
            Err(error) => {
                self.idle_requested = false;
                self.requeue_inflight_steering();
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

    fn handle_finalization_error(&mut self, error: RuntimeError) -> RuntimeError {
        self.idle_requested = false;
        self.requeue_inflight_steering();
        let team_requeue = self.requeue_inflight_team_messages();
        self.requeue_inflight_background_notifications();

        let message = error.to_string();
        self.set_status(AgentStatus::Failed(message.clone()));
        let _ = self.persist_agent_record();
        let _ = self.fail_run_checkpoint(&message);
        self.emit_event(AgentEvent::RunFailed { error: message });

        match team_requeue {
            Ok(()) => error,
            Err(requeue_error) => RuntimeError::Store(format!(
                "{error}; additionally failed to requeue the team inbox: {requeue_error}"
            )),
        }
    }

    pub(crate) fn request_idle(&mut self) {
        self.idle_requested = true;
    }

    pub(crate) fn take_idle_requested(&mut self) -> bool {
        std::mem::take(&mut self.idle_requested)
    }
}
