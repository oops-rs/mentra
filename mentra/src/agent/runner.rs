use std::{borrow::Cow, time::Duration};

use crate::{
    ContentBlock, Message,
    background::BackgroundNotification,
    error::RuntimeError,
    memory::journal::PendingTurnState,
    memory::{SearchRequest, build_search_query, recalled_memory_message},
    provider::Request,
    runtime::{RunOptions, RuntimeHookEvent, control::is_transient_provider_error},
    team::format_inbox,
    tool::ToolRuntime,
};

use super::{Agent, AgentEvent, AgentStatus, PendingAssistantTurn};

const PROVIDER_RETRY_BASE_DELAY: Duration = Duration::from_millis(500);
const PROVIDER_RETRY_MAX_DELAY: Duration = Duration::from_secs(5);
const MEMORY_SEARCH_TIMEOUT: Duration = Duration::from_millis(250);

pub(super) struct TurnRunner<'a> {
    agent: &'a mut Agent,
    options: RunOptions,
    model_requests: usize,
    tool_runtime: ToolRuntime,
}

impl<'a> TurnRunner<'a> {
    pub(super) fn new(agent: &'a mut Agent, options: RunOptions) -> Self {
        let tool_runtime = ToolRuntime::new(agent);
        Self {
            agent,
            options,
            model_requests: 0,
            tool_runtime,
        }
    }

    pub(super) async fn run(&mut self) -> Result<(), RuntimeError> {
        let mut rounds = 0usize;

        loop {
            self.options.check_limits()?;
            if let Some(limit) = self.agent.max_rounds()
                && rounds >= limit
            {
                return Err(RuntimeError::MaxRoundsExceeded(limit));
            }

            rounds += 1;
            self.agent.update_run_state("awaiting_model", None)?;
            let pending = self.stream_turn().await?;
            self.commit_assistant_message(&pending)?;

            let tool_calls = pending.ready_tool_calls()?;
            if tool_calls.is_empty() {
                self.agent.note_round_without_task();
                return Ok(());
            }

            let execution = self
                .tool_runtime
                .execute_calls(self.agent, &self.options, tool_calls)
                .await?;

            self.agent.memory.append_tool_results(execution.results)?;
            self.agent.sync_memory_snapshot();
            if execution.successful_task {
                self.agent.record_task_activity();
            } else {
                self.agent.note_round_without_task();
            }
            self.agent.persist_agent_record()?;
            if execution.end_turn {
                return Ok(());
            }
        }
    }

    async fn stream_turn(&mut self) -> Result<PendingAssistantTurn, RuntimeError> {
        self.agent.inject_team_inbox()?;
        self.agent.inject_background_notifications()?;
        self.agent.set_status(AgentStatus::AwaitingModel);
        self.agent.refresh_tasks_from_disk()?;
        self.agent.auto_compact_if_needed().await?;
        let provider = self.agent.provider.clone();
        let tools = self.agent.tools();
        let mut request_history = self.agent.micro_compacted_history();
        if let Some(recalled) = self.recalled_memory_message(&request_history).await {
            request_history.push(recalled);
        }
        self.agent.inject_teammate_identity(&mut request_history);
        if self.model_requests >= self.options.model_budget() {
            return Err(RuntimeError::ModelBudgetExceeded(
                self.options.model_budget(),
            ));
        }

        let request = Request {
            model: self.agent.model.as_str().into(),
            system: self.agent.effective_system_prompt(),
            messages: request_history.into(),
            tools: tools.as_ref().into(),
            tool_choice: self.agent.tool_choice(),
            temperature: self.agent.config.temperature,
            max_output_tokens: self.agent.config.max_output_tokens,
            metadata: Cow::Borrowed(&self.agent.config.metadata),
            provider_request_options: self.agent.config.provider_request_options.clone(),
        };
        let mut attempt = 0usize;
        let mut stream = loop {
            self.options.check_limits()?;
            attempt += 1;
            self.model_requests += 1;
            self.agent
                .runtime
                .emit_hook(RuntimeHookEvent::ModelRequestStarted {
                    agent_id: self.agent.id().to_string(),
                    model: self.agent.model().to_string(),
                    attempt,
                })?;
            match provider.stream(request.clone()).await {
                Ok(stream) => {
                    self.agent
                        .runtime
                        .emit_hook(RuntimeHookEvent::ModelRequestFinished {
                            agent_id: self.agent.id().to_string(),
                            model: self.agent.model().to_string(),
                            attempt,
                            success: true,
                            error: None,
                        })?;
                    break stream;
                }
                Err(error)
                    if attempt <= self.options.retry_budget
                        && is_transient_provider_error(&error) =>
                {
                    self.agent
                        .runtime
                        .emit_hook(RuntimeHookEvent::ModelRequestFinished {
                            agent_id: self.agent.id().to_string(),
                            model: self.agent.model().to_string(),
                            attempt,
                            success: false,
                            error: Some(error.to_string()),
                        })?;
                    if self.model_requests >= self.options.model_budget() {
                        return Err(RuntimeError::ModelBudgetExceeded(
                            self.options.model_budget(),
                        ));
                    }
                    tokio::time::sleep(provider_retry_delay(attempt)).await;
                    continue;
                }
                Err(error) => {
                    self.agent
                        .runtime
                        .emit_hook(RuntimeHookEvent::ModelRequestFinished {
                            agent_id: self.agent.id().to_string(),
                            model: self.agent.model().to_string(),
                            attempt,
                            success: false,
                            error: Some(error.to_string()),
                        })?;
                    return Err(RuntimeError::FailedToStreamResponse(error));
                }
            }
        };

        let mut pending = PendingAssistantTurn::default();
        self.agent.set_status(AgentStatus::Streaming);
        self.agent
            .memory
            .update_pending_turn(Self::pending_state(&pending))?;
        self.agent.sync_memory_snapshot();

        while let Some(event) = stream.recv().await {
            self.options.check_limits()?;
            let event = event.map_err(RuntimeError::FailedToStreamResponse)?;
            let derived_events = pending.apply(event)?;
            self.agent
                .memory
                .update_pending_turn(Self::pending_state(&pending))?;
            self.agent.sync_memory_snapshot();

            for event in derived_events {
                self.agent.emit_event(event);
            }
        }

        Ok(pending)
    }

    fn commit_assistant_message(
        &mut self,
        pending: &PendingAssistantTurn,
    ) -> Result<(), RuntimeError> {
        let assistant_message = pending.to_message()?;
        self.agent
            .memory
            .commit_assistant_message(assistant_message.clone())?;
        self.agent.sync_memory_snapshot();
        self.agent
            .emit_event(AgentEvent::AssistantMessageCommitted {
                message: assistant_message,
            });
        Ok(())
    }

    fn pending_state(pending: &PendingAssistantTurn) -> PendingTurnState {
        PendingTurnState::new(
            pending.current_text().to_string(),
            pending.pending_tool_use_summaries(),
        )
    }

    async fn recalled_memory_message(&self, request_history: &[Message]) -> Option<Message> {
        let query = build_search_query(request_history, self.agent.tasks());
        if query.trim().is_empty() {
            return None;
        }

        let memory = self.agent.runtime.memory_engine();
        let search = memory.search(SearchRequest {
            agent_id: self.agent.id().to_string(),
            query,
            limit: 3,
        });
        let hits = match tokio::time::timeout(MEMORY_SEARCH_TIMEOUT, search).await {
            Ok(Ok(hits)) => hits,
            Ok(Err(_error)) => return None,
            Err(_) => {
                let _ = self
                    .agent
                    .runtime
                    .emit_hook(RuntimeHookEvent::MemorySearchFinished {
                        agent_id: self.agent.id().to_string(),
                        success: false,
                        result_count: 0,
                        error: Some("memory search timed out".to_string()),
                    });
                return None;
            }
        };
        recalled_memory_message(&hits, 2_000)
    }
}

fn provider_retry_delay(attempt: usize) -> Duration {
    let shift = attempt.saturating_sub(1).min(usize::BITS as usize - 1) as u32;
    let factor = 1u32 << shift;
    PROVIDER_RETRY_BASE_DELAY
        .checked_mul(factor)
        .unwrap_or(PROVIDER_RETRY_MAX_DELAY)
        .min(PROVIDER_RETRY_MAX_DELAY)
}

impl Agent {
    pub(super) fn inject_team_inbox(&mut self) -> Result<(), RuntimeError> {
        let messages = self
            .runtime
            .read_team_inbox(self.config.team.team_dir.as_path(), &self.name)?;
        if messages.is_empty() {
            return Ok(());
        }

        self.inflight_team_messages.extend(messages.iter().cloned());
        self.memory
            .append_message(Message::user(ContentBlock::text(format_inbox(&messages))))?;
        self.sync_memory_snapshot();
        Ok(())
    }

    pub(super) fn clear_inflight_team_messages(&mut self) {
        let _ = self
            .runtime
            .acknowledge_team_messages(self.config.team.team_dir.as_path(), &self.name);
        self.inflight_team_messages.clear();
    }

    pub(super) fn requeue_inflight_team_messages(&mut self) -> Result<(), RuntimeError> {
        let messages = std::mem::take(&mut self.inflight_team_messages);
        self.runtime.requeue_team_messages(
            self.config.team.team_dir.as_path(),
            &self.name,
            messages,
        )
    }

    pub(super) fn inject_background_notifications(&mut self) -> Result<(), RuntimeError> {
        let notifications = self.runtime.drain_background_notifications(&self.id);
        if notifications.is_empty() {
            return Ok(());
        }

        self.inflight_background_notifications
            .extend(notifications.iter().cloned());
        self.memory
            .append_message(Message::user(ContentBlock::text(
                format_background_results(&notifications),
            )))?;
        self.sync_memory_snapshot();
        Ok(())
    }

    pub(super) fn clear_inflight_background_notifications(&mut self) {
        self.runtime.acknowledge_background_notifications(&self.id);
        self.inflight_background_notifications.clear();
    }

    pub(super) fn requeue_inflight_background_notifications(&mut self) {
        let notifications = std::mem::take(&mut self.inflight_background_notifications);
        self.runtime
            .requeue_background_notifications(&self.id, notifications);
    }
}

fn format_background_results(notifications: &[BackgroundNotification]) -> String {
    let lines = notifications
        .iter()
        .map(|notification| {
            format!(
                "[bg:{}] status={} command=\"{}\" output=\"{}\"",
                notification.task_id,
                notification.status,
                escape_background_field(&notification.command),
                escape_background_field(&notification.output_preview),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!("<background-results>\n{lines}\n</background-results>")
}

fn escape_background_field(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}
