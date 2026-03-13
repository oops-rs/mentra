use std::{borrow::Cow, time::Duration};

use tokio::task::JoinSet;

use crate::{
    ContentBlock, Message,
    background::BackgroundNotification,
    error::RuntimeError,
    provider::Request,
    runtime::{RunOptions, RuntimeHookEvent, control::is_transient_provider_error},
    team::format_inbox,
    tool::{
        ParallelToolContext, ToolCall, ToolCapability, ToolContext, ToolExecutionMode, ToolSpec,
    },
};

use super::{Agent, AgentEvent, AgentStatus, PendingAssistantTurn, memory::PendingTurnState};

pub(super) struct TurnRunner<'a> {
    agent: &'a mut Agent,
    options: RunOptions,
    model_requests: usize,
    tool_calls: usize,
}

const PARALLEL_JOIN_POLL_INTERVAL: Duration = Duration::from_millis(10);

struct CompletedToolExecution {
    result: ContentBlock,
    task_succeeded: bool,
    should_end_turn: bool,
}

impl<'a> TurnRunner<'a> {
    pub(super) fn new(agent: &'a mut Agent, options: RunOptions) -> Self {
        Self {
            agent,
            options,
            model_requests: 0,
            tool_calls: 0,
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

            let mut tool_results = Vec::new();
            let mut successful_task = false;
            let mut end_turn = false;
            let mut index = 0usize;
            while index < tool_calls.len() {
                self.options.check_limits()?;

                let batch_len = self.parallel_batch_len(&tool_calls[index..]);
                let execution_count = if batch_len > 0 { batch_len } else { 1 };
                if self.tool_calls + execution_count > self.options.tool_budget() {
                    return Err(RuntimeError::ToolBudgetExceeded(self.options.tool_budget()));
                }
                self.tool_calls += execution_count;

                if batch_len > 0 {
                    let executions = self
                        .execute_parallel_batch(tool_calls[index..index + batch_len].to_vec())
                        .await?;
                    for execution in executions {
                        successful_task |= execution.task_succeeded;
                        end_turn |= execution.should_end_turn;
                        tool_results.push(execution.result);
                    }
                    index += batch_len;
                    continue;
                }

                let call = tool_calls[index].clone();
                let execution = self.execute_one_tool(call).await?;
                successful_task |= execution.task_succeeded;
                end_turn |= execution.should_end_turn;
                tool_results.push(execution.result);
                index += 1;
            }

            self.agent.memory.append_tool_results(tool_results)?;
            self.agent.sync_memory_snapshot();
            if successful_task {
                self.agent.record_task_activity();
            } else {
                self.agent.note_round_without_task();
            }
            self.agent.persist_agent_record()?;
            if end_turn {
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

    fn unavailable_tool_result(&self, call: crate::tool::ToolCall) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Tool '{}' is not available for this agent", call.name),
            is_error: true,
        }
    }

    fn parallel_batch_len(&self, calls: &[ToolCall]) -> usize {
        calls
            .iter()
            .take_while(|call| self.call_execution_mode(call) == ToolExecutionMode::Parallel)
            .count()
    }

    fn call_execution_mode(&self, call: &ToolCall) -> ToolExecutionMode {
        if !self.agent.can_use_tool(&call.name) {
            return ToolExecutionMode::Exclusive;
        }

        self.agent
            .runtime
            .get_tool(&call.name)
            .map(|tool| tool.execution_mode(&call.input))
            .unwrap_or(ToolExecutionMode::Exclusive)
    }

    fn note_tool_started(&mut self, call: &ToolCall) -> Result<(), RuntimeError> {
        self.agent.set_status(AgentStatus::ExecutingTool {
            id: call.id.clone(),
            name: call.name.clone(),
        });
        self.agent
            .emit_event(AgentEvent::ToolExecutionStarted { call: call.clone() });
        self.agent.update_run_state("executing_tool", None)
    }

    fn emit_tool_runtime_started(&self, call: &ToolCall) -> Result<(), RuntimeError> {
        self.agent
            .runtime
            .emit_hook(RuntimeHookEvent::ToolExecutionStarted {
                agent_id: self.agent.id().to_string(),
                tool_name: call.name.clone(),
                tool_call_id: call.id.clone(),
            })
    }

    fn emit_tool_runtime_finished(&self, call: &ToolCall, result: &ContentBlock) {
        let is_error = matches!(result, ContentBlock::ToolResult { is_error: true, .. });
        let output_preview = match result {
            ContentBlock::ToolResult { content, .. } => content.clone(),
            _ => String::new(),
        };
        let error = is_error.then_some(output_preview.clone());
        let _ = self
            .agent
            .runtime
            .emit_hook(RuntimeHookEvent::ToolExecutionFinished {
                agent_id: self.agent.id().to_string(),
                tool_name: call.name.clone(),
                tool_call_id: call.id.clone(),
                is_error,
                error,
                output_preview,
            });
    }

    fn blocked_tool_result(&self, call: &ToolCall, error: RuntimeError) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_use_id: call.id.clone(),
            content: format!("Tool execution blocked: {error}"),
            is_error: true,
        }
    }

    fn tool_result_block(call: &ToolCall, result: crate::tool::ToolResult) -> ContentBlock {
        match result {
            Ok(content) => ContentBlock::ToolResult {
                tool_use_id: call.id.clone(),
                content,
                is_error: false,
            },
            Err(content) => ContentBlock::ToolResult {
                tool_use_id: call.id.clone(),
                content,
                is_error: true,
            },
        }
    }

    fn completed_execution(
        &self,
        call: &ToolCall,
        spec: &ToolSpec,
        result: ContentBlock,
        should_end_turn: bool,
    ) -> CompletedToolExecution {
        self.emit_tool_runtime_finished(call, &result);
        self.agent.emit_event(AgentEvent::ToolExecutionFinished {
            result: result.clone(),
        });
        let task_succeeded = matches!(
            &result,
            ContentBlock::ToolResult {
                is_error: false,
                ..
            }
        ) && spec
            .capabilities
            .iter()
            .any(|capability| matches!(capability, ToolCapability::TaskMutation));

        CompletedToolExecution {
            result,
            task_succeeded,
            should_end_turn,
        }
    }

    fn parallel_tool_context(&self, call: &ToolCall) -> ParallelToolContext {
        let working_directory = self
            .agent
            .runtime
            .resolve_working_directory(self.agent.id(), None)
            .unwrap_or_else(|_| {
                self.agent
                    .runtime
                    .default_working_directory(self.agent.id())
            });

        ParallelToolContext {
            agent_id: self.agent.id().to_string(),
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            working_directory,
            runtime: self.agent.runtime.clone(),
            agent_name: self.agent.name().to_string(),
            model: self.agent.model().to_string(),
            history_len: self.agent.history().len(),
            tasks: self.agent.tasks().to_vec(),
        }
    }

    async fn execute_one_tool(
        &mut self,
        call: ToolCall,
    ) -> Result<CompletedToolExecution, RuntimeError> {
        self.note_tool_started(&call)?;
        if !self.agent.can_use_tool(&call.name) {
            let result = self.unavailable_tool_result(call.clone());
            self.agent.emit_event(AgentEvent::ToolExecutionFinished {
                result: result.clone(),
            });
            return Ok(CompletedToolExecution {
                result,
                task_succeeded: false,
                should_end_turn: false,
            });
        }

        Ok(self.execute_registered_tool(call).await)
    }

    async fn execute_parallel_batch(
        &mut self,
        calls: Vec<ToolCall>,
    ) -> Result<Vec<CompletedToolExecution>, RuntimeError> {
        let len = calls.len();
        let mut results = (0..len).map(|_| None).collect::<Vec<_>>();
        let mut join_set = JoinSet::new();

        for (index, call) in calls.iter().cloned().enumerate() {
            if let Err(error) = self.note_tool_started(&call) {
                join_set.abort_all();
                return Err(error);
            }

            let Some(tool) = self.agent.runtime.get_tool(&call.name) else {
                let result = ContentBlock::ToolResult {
                    tool_use_id: call.id.clone(),
                    content: "Tool not found".to_string(),
                    is_error: true,
                };
                self.agent.emit_event(AgentEvent::ToolExecutionFinished {
                    result: result.clone(),
                });
                results[index] = Some(CompletedToolExecution {
                    result,
                    task_succeeded: false,
                    should_end_turn: false,
                });
                continue;
            };
            let spec = tool.spec();

            if let Err(error) = self.emit_tool_runtime_started(&call) {
                let result = self.blocked_tool_result(&call, error);
                let execution = self.completed_execution(&call, &spec, result, false);
                results[index] = Some(execution);
                continue;
            }

            let ctx = self.parallel_tool_context(&call);
            join_set.spawn(async move {
                let result = tool.execute(ctx, call.input.clone()).await;
                (index, call, spec, result)
            });
        }

        while !join_set.is_empty() {
            if let Err(error) = self.options.check_limits() {
                join_set.abort_all();
                return Err(error);
            }
            match tokio::time::timeout(PARALLEL_JOIN_POLL_INTERVAL, join_set.join_next()).await {
                Ok(Some(Ok((index, call, spec, result)))) => {
                    let result = Self::tool_result_block(&call, result);
                    results[index] = Some(self.completed_execution(&call, &spec, result, false));
                }
                Ok(Some(Err(error))) => {
                    join_set.abort_all();
                    return Err(RuntimeError::Store(format!(
                        "parallel tool task failed: {error}"
                    )));
                }
                Ok(None) => break,
                Err(_) => continue,
            }
        }

        if let Err(error) = self.options.check_limits() {
            join_set.abort_all();
            return Err(error);
        }

        let mut ordered = Vec::with_capacity(len);
        for result in results {
            ordered.push(result.ok_or_else(|| {
                RuntimeError::Store("parallel tool batch lost a result".to_string())
            })?);
        }

        Ok(ordered)
    }

    async fn execute_registered_tool(&mut self, call: ToolCall) -> CompletedToolExecution {
        let Some(tool) = self.agent.runtime.get_tool(&call.name) else {
            let result = ContentBlock::ToolResult {
                tool_use_id: call.id.clone(),
                content: "Tool not found".to_string(),
                is_error: true,
            };
            self.agent.emit_event(AgentEvent::ToolExecutionFinished {
                result: result.clone(),
            });
            return CompletedToolExecution {
                result,
                task_succeeded: false,
                should_end_turn: false,
            };
        };
        let spec = tool.spec();

        let working_directory = self
            .agent
            .runtime
            .resolve_working_directory(self.agent.id(), None)
            .unwrap_or_else(|_| {
                self.agent
                    .runtime
                    .default_working_directory(self.agent.id())
            });
        if let Err(error) = self.emit_tool_runtime_started(&call) {
            let result = self.blocked_tool_result(&call, error);
            return self.completed_execution(&call, &spec, result, false);
        }

        let result = Self::tool_result_block(
            &call,
            tool.execute_mut(
                ToolContext {
                    agent_id: self.agent.id().to_string(),
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    working_directory,
                    runtime: self.agent.runtime.clone(),
                    agent: self.agent,
                },
                call.input.clone(),
            )
            .await,
        );
        let should_end_turn = self.agent.take_idle_requested();
        self.completed_execution(&call, &spec, result, should_end_turn)
    }
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
