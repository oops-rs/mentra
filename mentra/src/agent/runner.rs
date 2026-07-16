use std::{borrow::Cow, collections::HashMap, sync::Arc, time::Duration};

use crate::{
    ContentBlock, Message, Role,
    background::BackgroundNotification,
    error::RuntimeError,
    memory::journal::PendingTurnState,
    memory::{MemorySearchMode, MemorySearchRequest, build_search_query, recalled_memory_message},
    provider::Request,
    runtime::{RunOptions, RuntimeHookEvent, control::is_transient_provider_error},
    team::format_inbox,
    tool::{ToolCall, ToolRuntime},
    transcript::{DelegationArtifact, DelegationKind, DelegationStatus},
};

use super::{
    Agent, AgentEvent, AgentStatus, PendingAssistantTurn,
    pending::InvalidToolUse,
    round_strategy::{
        ReasoningChange, RoundAdjustment, RoundBoundary, RoundContext, RoundDecision,
        RoundStrategy, RoundToolResult,
    },
};

/// How the round loop should proceed after a [`RoundStrategy`] decision.
enum RoundFlow {
    /// Advance to the next round (or, at the assistant boundary, return).
    Continue,
    /// A corrective turn was injected; run another round.
    Inject,
    /// End the run gracefully at this boundary.
    Stop,
}

const PROVIDER_RETRY_BASE_DELAY: Duration = Duration::from_millis(500);
const PROVIDER_RETRY_MAX_DELAY: Duration = Duration::from_secs(5);
const MEMORY_SEARCH_TIMEOUT: Duration = Duration::from_millis(250);

pub(super) struct TurnRunner<'a> {
    agent: &'a mut Agent,
    options: RunOptions,
    model_requests: usize,
    /// Transient transport-connection retries made so far, counted separately
    /// from `model_requests` (which includes them) and from the loop's `rounds`
    /// counter (which counts only completed logical rounds). See
    /// [`RoundContext::transport_retries`](crate::agent::RoundContext::transport_retries).
    transport_retries: usize,
    tool_runtime: ToolRuntime,
}

struct StreamedTurn {
    attempt: usize,
    pending: PendingAssistantTurn,
}

impl<'a> TurnRunner<'a> {
    pub(super) fn new(agent: &'a mut Agent, options: RunOptions) -> Self {
        let tool_runtime = ToolRuntime::new(agent);
        Self {
            agent,
            options,
            model_requests: 0,
            transport_retries: 0,
            tool_runtime,
        }
    }

    pub(super) async fn run(&mut self) -> Result<(), RuntimeError> {
        let mut rounds = 0usize;

        loop {
            self.options.check_limits()?;
            // Graceful stop: end the run successfully at this round boundary (where
            // the transcript is consistent), keeping the committed work, rather than
            // failing and rolling back the way `cancellation` does. Lets a caller
            // stop gathering once enough is done while preserving the context for a
            // follow-up turn on the same agent.
            if self.options.stop_requested() {
                return Ok(());
            }
            // Soft aggregate token bound: usage is only known once a round has
            // completed, so this never preempts a round in progress — it only
            // refuses to start another one once the last round's reported usage
            // pushed the cumulative total to or past the bound. The transcript
            // through the last completed round stays committed, exactly like
            // `stop` above.
            if self.options.token_budget_exceeded() {
                return Ok(());
            }
            if let Some(limit) = self.agent.max_rounds() {
                if rounds >= limit {
                    return Err(RuntimeError::MaxRoundsExceeded(limit));
                }
            }

            rounds += 1;
            self.agent.update_run_state("awaiting_model", None)?;
            let streamed = self.stream_turn().await?;
            let invalid_tool_uses = streamed.pending.invalid_tool_uses().to_vec();
            if let Err(error) = self.commit_assistant_message(&streamed.pending) {
                self.emit_model_response_finished(
                    streamed.attempt,
                    false,
                    Some(error.to_string()),
                    None,
                    None,
                )?;
                return Err(error);
            }
            let usage = streamed.pending.usage().cloned();
            self.emit_model_response_finished(
                streamed.attempt,
                true,
                None,
                streamed.pending.stop_reason().map(str::to_string),
                usage.clone(),
            )?;

            // Emit token usage report if available.
            if let Some(ref u) = usage {
                self.agent.emit_event(AgentEvent::UsageReport {
                    input_tokens: u.input_tokens.unwrap_or(0),
                    output_tokens: u.output_tokens.unwrap_or(0),
                    cache_read_tokens: u.cache_read_input_tokens.unwrap_or(0),
                    cache_creation_tokens: u.cache_creation_input_tokens.unwrap_or(0),
                });
                self.options
                    .record_tokens(u.input_tokens.unwrap_or(0) + u.output_tokens.unwrap_or(0));
            }

            if !invalid_tool_uses.is_empty() {
                self.append_invalid_tool_input_feedback(&invalid_tool_uses)?;
                self.agent.note_round_without_task();
                self.agent.persist_agent_record()?;
                continue;
            }

            let tool_calls = streamed.pending.ready_tool_calls()?;
            if tool_calls.is_empty() {
                if let Some(strategy) = self.options.round_strategy.clone() {
                    let assistant_message = self
                        .agent
                        .last_message()
                        .filter(|message| message.role == Role::Assistant)
                        .cloned();
                    let flow = self
                        .run_round_strategy(
                            &strategy,
                            RoundBoundary::AssistantMessageCommitted,
                            assistant_message.as_ref(),
                            &[],
                            rounds,
                        )
                        .await?;
                    match flow {
                        RoundFlow::Inject => {
                            self.agent.note_round_without_task();
                            self.agent.persist_agent_record()?;
                            continue;
                        }
                        RoundFlow::Continue | RoundFlow::Stop => {
                            self.agent.note_round_without_task();
                            return Ok(());
                        }
                    }
                }
                self.agent.note_round_without_task();
                return Ok(());
            }

            let round_strategy = self.options.round_strategy.clone();
            let call_names = round_strategy
                .as_ref()
                .map(|_| collect_tool_call_names(&tool_calls));

            let execution = self
                .tool_runtime
                .execute_calls(self.agent, &self.options, tool_calls)
                .await?;

            let tool_results = call_names
                .map(|names| summarize_tool_results(&execution.results, &names))
                .unwrap_or_default();

            let tool_result_message = Message {
                role: Role::User,
                content: execution.results,
            };
            if execution.details.is_empty() {
                self.agent.memory.append_message(tool_result_message)?;
            } else {
                self.agent
                    .memory
                    .append_message_with_details(tool_result_message, execution.details)?;
            }
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

            if let Some(strategy) = round_strategy {
                let flow = self
                    .run_round_strategy(
                        &strategy,
                        RoundBoundary::ToolResultsCommitted,
                        None,
                        &tool_results,
                        rounds,
                    )
                    .await?;
                if matches!(flow, RoundFlow::Stop) {
                    return Ok(());
                }
            }
        }
    }

    /// Invokes the per-run [`RoundStrategy`] at a round boundary and applies its
    /// decision: any model/reasoning switch, then any corrective injection. The
    /// returned [`RoundFlow`] tells the loop whether to continue, treat the round
    /// as injected, or stop gracefully.
    async fn run_round_strategy(
        &mut self,
        strategy: &Arc<dyn RoundStrategy>,
        boundary: RoundBoundary,
        assistant_message: Option<&Message>,
        tool_results: &[RoundToolResult],
        rounds: usize,
    ) -> Result<RoundFlow, RuntimeError> {
        let context = RoundContext::new(
            boundary,
            assistant_message,
            tool_results,
            rounds,
            self.model_requests,
            self.transport_retries,
        );
        match strategy.on_round(context).await {
            RoundDecision::Continue(adjustment) => {
                self.apply_round_adjustment(adjustment)?;
                Ok(RoundFlow::Continue)
            }
            RoundDecision::Inject { content, adjust } => {
                self.apply_round_adjustment(adjust)?;
                self.inject_round_context(content)?;
                Ok(RoundFlow::Inject)
            }
            RoundDecision::Stop => Ok(RoundFlow::Stop),
        }
    }

    /// Applies a strategy-requested model/reasoning switch to subsequent rounds,
    /// reusing the live-config mechanics of [`Agent::set_model`] and
    /// [`Agent::set_reasoning`] (which `stream_turn` reads live on the next round).
    fn apply_round_adjustment(&mut self, adjustment: RoundAdjustment) -> Result<(), RuntimeError> {
        if let Some(model) = adjustment.model {
            self.agent.set_model(model)?;
        }
        match adjustment.reasoning {
            Some(ReasoningChange::Set(options)) => self.agent.set_reasoning(Some(options))?,
            Some(ReasoningChange::Clear) => self.agent.set_reasoning(None)?,
            None => {}
        }
        Ok(())
    }

    /// Appends strategy-supplied corrective context as a committed user turn,
    /// mirroring [`append_invalid_tool_input_feedback`](Self::append_invalid_tool_input_feedback)
    /// so the injection is part of the replayable transcript.
    fn inject_round_context(&mut self, content: Vec<ContentBlock>) -> Result<(), RuntimeError> {
        self.agent.memory.append_message(Message {
            role: Role::User,
            content,
        })?;
        self.agent.sync_memory_snapshot();
        Ok(())
    }

    async fn stream_turn(&mut self) -> Result<StreamedTurn, RuntimeError> {
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
                    self.transport_retries += 1;
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
                    let delay = provider_retry_delay(attempt);
                    self.agent.emit_event(AgentEvent::RetryAttempt {
                        agent_id: self.agent.id().to_string(),
                        error_message: error.to_string(),
                        attempt: attempt as u32,
                        max_attempts: self.options.retry_budget as u32,
                        next_delay_ms: delay.as_millis() as u64,
                    });
                    tokio::time::sleep(delay).await;
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
            if let Err(error) = self.options.check_limits() {
                self.emit_model_response_finished(
                    attempt,
                    false,
                    Some(error.to_string()),
                    None,
                    None,
                )?;
                return Err(error);
            }
            let event = match event {
                Ok(event) => event,
                Err(error) => {
                    let runtime_error = RuntimeError::FailedToStreamResponse(error);
                    let error_message = runtime_error.to_string();
                    self.emit_model_response_finished(
                        attempt,
                        false,
                        Some(error_message),
                        None,
                        None,
                    )?;
                    return Err(runtime_error);
                }
            };
            let derived_events = match pending.apply(event) {
                Ok(derived_events) => derived_events,
                Err(error) => {
                    self.emit_model_response_finished(
                        attempt,
                        false,
                        Some(error.to_string()),
                        None,
                        None,
                    )?;
                    return Err(error);
                }
            };
            self.agent
                .memory
                .update_pending_turn(Self::pending_state(&pending))?;
            self.agent.sync_memory_snapshot();

            for event in derived_events {
                self.agent.emit_event(event);
            }
        }

        Ok(StreamedTurn { attempt, pending })
    }

    fn commit_assistant_message(
        &mut self,
        pending: &PendingAssistantTurn,
    ) -> Result<(), RuntimeError> {
        let assistant_message = pending.to_message()?;
        if assistant_message.content.is_empty() {
            self.agent.memory.clear_pending_turn()?;
            self.agent.sync_memory_snapshot();
            return Ok(());
        }
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

    fn append_invalid_tool_input_feedback(
        &mut self,
        invalid_tool_uses: &[InvalidToolUse],
    ) -> Result<(), RuntimeError> {
        self.agent
            .memory
            .append_message(Message::user(ContentBlock::text(
                format_invalid_tool_input_feedback(invalid_tool_uses),
            )))?;
        self.agent.sync_memory_snapshot();
        Ok(())
    }

    fn pending_state(pending: &PendingAssistantTurn) -> PendingTurnState {
        PendingTurnState::new(
            pending.current_text().to_string(),
            pending.pending_tool_use_summaries(),
        )
    }

    fn emit_model_response_finished(
        &self,
        attempt: usize,
        success: bool,
        error: Option<String>,
        stop_reason: Option<String>,
        usage: Option<crate::provider::TokenUsage>,
    ) -> Result<(), RuntimeError> {
        self.agent
            .runtime
            .emit_hook(RuntimeHookEvent::ModelResponseFinished {
                agent_id: self.agent.id().to_string(),
                model: self.agent.model().to_string(),
                attempt,
                success,
                error,
                stop_reason,
                usage,
            })
    }

    async fn recalled_memory_message(&self, request_history: &[Message]) -> Option<Message> {
        if !self.agent.config().memory.auto_recall_enabled {
            return None;
        }
        let query = build_search_query(request_history, self.agent.tasks());
        if query.trim().is_empty() {
            return None;
        }

        let memory = self.agent.runtime.memory_engine();
        let search = memory.search(MemorySearchRequest {
            agent_id: self.agent.id().to_string(),
            query,
            limit: self.agent.config().memory.auto_recall_limit,
            char_budget: Some(self.agent.config().memory.auto_recall_char_budget),
            mode: MemorySearchMode::Automatic,
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
        recalled_memory_message(&hits, self.agent.config().memory.auto_recall_char_budget)
    }
}

/// Builds a `tool_use_id -> tool_name` map from the round's tool calls so a
/// committed tool result (which carries only `tool_use_id`) can be summarized
/// with its originating tool name for a [`RoundStrategy`].
fn collect_tool_call_names(calls: &[ToolCall]) -> HashMap<String, String> {
    calls
        .iter()
        .map(|call| (call.id.clone(), call.name.clone()))
        .collect()
}

/// Summarizes committed tool-result blocks into provider-neutral
/// [`RoundToolResult`]s for a [`RoundStrategy`], correlating each result with its
/// originating tool name via `names`.
fn summarize_tool_results(
    results: &[ContentBlock],
    names: &HashMap<String, String>,
) -> Vec<RoundToolResult> {
    results
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                is_error,
                ..
            } => Some(RoundToolResult {
                tool_use_id: tool_use_id.clone(),
                tool_name: names.get(tool_use_id).cloned().unwrap_or_default(),
                is_error: *is_error,
            }),
            _ => None,
        })
        .collect()
}

fn provider_retry_delay(attempt: usize) -> Duration {
    let shift = attempt.saturating_sub(1).min(usize::BITS as usize - 1) as u32;
    let factor = 1u32 << shift;
    PROVIDER_RETRY_BASE_DELAY
        .checked_mul(factor)
        .unwrap_or(PROVIDER_RETRY_MAX_DELAY)
        .min(PROVIDER_RETRY_MAX_DELAY)
}

fn format_invalid_tool_input_feedback(invalid_tool_uses: &[InvalidToolUse]) -> String {
    let mut feedback = String::from(
        "One or more tool calls could not be executed because their JSON arguments were invalid. \
Please retry with valid JSON that matches the tool schema exactly.\n\n",
    );

    for invalid in invalid_tool_uses {
        feedback.push_str(&format!(
            "Tool '{}' ({}) failed to parse: {}.\nRaw arguments (truncated): {}\n\n",
            invalid.name,
            invalid.id,
            invalid.error,
            truncate_tool_input(&invalid.input_json, 240)
        ));
    }

    feedback.truncate(feedback.trim_end().len());
    feedback
}

fn truncate_tool_input(input: &str, max_chars: usize) -> String {
    let mut truncated = input.chars().take(max_chars).collect::<String>();
    if input.chars().count() > max_chars {
        truncated.push_str("...");
    }
    truncated
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
        for message in &messages {
            let content = format_inbox(std::slice::from_ref(message));
            self.record_delegation_request(
                content,
                DelegationArtifact {
                    kind: DelegationKind::Teammate,
                    agent_id: message.sender.clone(),
                    agent_name: message.sender.clone(),
                    role: Some("teammate".to_string()),
                    status: DelegationStatus::Requested,
                    task_summary: message.content.clone(),
                    result_summary: None,
                    artifacts: Vec::new(),
                },
                None,
            )?;
        }
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
        self.record_canonical_context(format_background_results(&notifications))?;
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
