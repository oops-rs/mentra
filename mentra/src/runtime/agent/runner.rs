use std::borrow::Cow;

use crate::{
    ContentBlock, Message, Role,
    provider::Request,
    runtime::{
        RunOptions, RuntimeHookEvent, background::BackgroundNotification,
        control::is_transient_provider_error, error::RuntimeError, team::format_inbox,
    },
    tool::{ToolCapability, ToolContext},
};

use super::{Agent, AgentEvent, AgentStatus, PendingAssistantTurn};

pub(super) struct TurnRunner<'a> {
    agent: &'a mut Agent,
    options: RunOptions,
    model_requests: usize,
    tool_calls: usize,
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
            for call in tool_calls {
                self.options.check_limits()?;
                if self.tool_calls >= self.options.tool_budget() {
                    return Err(RuntimeError::ToolBudgetExceeded(self.options.tool_budget()));
                }
                self.tool_calls += 1;
                self.agent.set_status(AgentStatus::ExecutingTool {
                    id: call.id.clone(),
                    name: call.name.clone(),
                });
                self.agent
                    .emit_event(AgentEvent::ToolExecutionStarted { call: call.clone() });
                self.agent.update_run_state("executing_tool", None)?;

                let (result, task_succeeded, should_end_turn) =
                    if !self.agent.can_use_tool(&call.name) {
                        (self.unavailable_tool_result(call), false, false)
                    } else {
                        self.execute_registered_tool(call).await
                    };
                self.agent.emit_event(AgentEvent::ToolExecutionFinished {
                    result: result.clone(),
                });
                successful_task |= task_succeeded;
                end_turn |= should_end_turn;
                tool_results.push(result);
            }

            self.agent.push_history(Message {
                role: Role::User,
                content: tool_results,
            });
            self.agent.persist_state()?;
            if successful_task {
                self.agent.record_task_activity();
            } else {
                self.agent.note_round_without_task();
            }
            self.agent.clear_pending_turn();
            self.agent.clear_persisted_pending_turn()?;
            if end_turn {
                return Ok(());
            }
        }
    }

    async fn stream_turn(&mut self) -> Result<PendingAssistantTurn, RuntimeError> {
        self.agent.inject_team_inbox()?;
        self.agent.inject_background_notifications();
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
        self.agent.publish_pending_turn(&pending);
        self.agent.persist_pending_turn(&pending)?;

        while let Some(event) = stream.recv().await {
            self.options.check_limits()?;
            let event = event.map_err(RuntimeError::FailedToStreamResponse)?;
            let derived_events = pending.apply(event)?;
            self.agent.publish_pending_turn(&pending);
            self.agent.persist_pending_turn(&pending)?;

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
        self.agent.push_history(assistant_message.clone());
        self.agent.persist_state()?;
        self.agent.clear_pending_turn();
        self.agent.clear_persisted_pending_turn()?;
        self.agent
            .emit_event(AgentEvent::AssistantMessageCommitted {
                message: assistant_message,
            });
        Ok(())
    }

    fn unavailable_tool_result(&self, call: crate::tool::ToolCall) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Tool '{}' is not available for this agent", call.name),
            is_error: true,
        }
    }

    async fn execute_registered_tool(
        &mut self,
        call: crate::tool::ToolCall,
    ) -> (ContentBlock, bool, bool) {
        let Some(tool) = self.agent.runtime.get_tool(&call.name) else {
            return (
                ContentBlock::ToolResult {
                    tool_use_id: call.id,
                    content: "Tool not found".to_string(),
                    is_error: true,
                },
                false,
                false,
            );
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
        if let Err(error) = self
            .agent
            .runtime
            .emit_hook(RuntimeHookEvent::ToolExecutionStarted {
                agent_id: self.agent.id().to_string(),
                tool_name: call.name.clone(),
                tool_call_id: call.id.clone(),
            })
        {
            return (
                ContentBlock::ToolResult {
                    tool_use_id: call.id,
                    content: format!("Tool execution blocked: {error}"),
                    is_error: true,
                },
                false,
                false,
            );
        }

        let result = match tool
            .execute(
                ToolContext {
                    agent_id: self.agent.id().to_string(),
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    working_directory,
                    runtime: self.agent.runtime.clone(),
                    agent: self.agent,
                },
                call.input,
            )
            .await
        {
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
        };
        let is_error = matches!(&result, ContentBlock::ToolResult { is_error: true, .. });
        let output_preview = match &result {
            ContentBlock::ToolResult { content, .. } => content.clone(),
            _ => String::new(),
        };
        let error = if is_error {
            Some(output_preview.clone())
        } else {
            None
        };
        let _ = self
            .agent
            .runtime
            .emit_hook(RuntimeHookEvent::ToolExecutionFinished {
                agent_id: self.agent.id().to_string(),
                tool_name: call.name,
                tool_call_id: call.id,
                is_error,
                error,
                output_preview,
            });
        let touched_task = !is_error
            && spec
                .capabilities
                .iter()
                .any(|capability| matches!(capability, ToolCapability::TaskMutation));
        let should_end_turn = self.agent.take_idle_requested();
        (result, touched_task, should_end_turn)
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
        self.push_history(Message::user(ContentBlock::text(format_inbox(&messages))));
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

    pub(super) fn inject_background_notifications(&mut self) {
        let notifications = self.runtime.drain_background_notifications(&self.id);
        if notifications.is_empty() {
            return;
        }

        self.inflight_background_notifications
            .extend(notifications.iter().cloned());
        self.push_history(Message::user(ContentBlock::text(
            format_background_results(&notifications),
        )));
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
                notification.status.as_str(),
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
