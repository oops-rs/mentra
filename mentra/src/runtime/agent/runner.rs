use std::borrow::Cow;

use crate::{
    ContentBlock, Message, Role,
    provider::Request,
    runtime::{
        background::BackgroundNotification, error::RuntimeError, intrinsic, team::format_inbox,
    },
};

use super::{Agent, AgentEvent, AgentStatus, PendingAssistantTurn};

pub(super) struct TurnRunner<'a> {
    agent: &'a mut Agent,
}

impl<'a> TurnRunner<'a> {
    pub(super) fn new(agent: &'a mut Agent) -> Self {
        Self { agent }
    }

    pub(super) async fn run(&mut self) -> Result<(), RuntimeError> {
        let mut rounds = 0usize;

        loop {
            if let Some(limit) = self.agent.max_rounds()
                && rounds >= limit
            {
                return Err(RuntimeError::MaxRoundsExceeded(limit));
            }

            rounds += 1;
            let pending = self.stream_turn().await?;
            self.commit_assistant_message(&pending)?;

            let tool_calls = pending.ready_tool_calls()?;
            if tool_calls.is_empty() {
                self.agent.note_round_without_task_graph();
                return Ok(());
            }

            let mut tool_results = Vec::new();
            let mut successful_task_graph = false;
            for call in tool_calls {
                self.agent.set_status(AgentStatus::ExecutingTool {
                    id: call.id.clone(),
                    name: call.name.clone(),
                });
                self.agent
                    .emit_event(AgentEvent::ToolExecutionStarted { call: call.clone() });

                let (result, task_graph_succeeded) = if !self.agent.can_use_tool(&call.name) {
                    (self.unavailable_tool_result(call), false)
                } else if let Some(outcome) = intrinsic::execute(self.agent, call.clone()).await {
                    (outcome.result, outcome.touched_task_graph)
                } else {
                    (
                        self.agent.runtime.execute_tool(self.agent.id(), call).await,
                        false,
                    )
                };
                self.agent.emit_event(AgentEvent::ToolExecutionFinished {
                    result: result.clone(),
                });
                successful_task_graph |= task_graph_succeeded;
                tool_results.push(result);
            }

            self.agent.push_history(Message {
                role: Role::User,
                content: tool_results,
            });
            if successful_task_graph {
                self.agent.record_task_graph_activity();
            } else {
                self.agent.note_round_without_task_graph();
            }
            self.agent.clear_pending_turn();
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
        let request_history = self.agent.micro_compacted_history();
        let mut stream = {
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
            provider
                .stream(request)
                .await
                .map_err(RuntimeError::FailedToStreamResponse)?
        };

        let mut pending = PendingAssistantTurn::default();
        self.agent.set_status(AgentStatus::Streaming);
        self.agent.publish_pending_turn(&pending);

        while let Some(event) = stream.recv().await {
            let event = event.map_err(RuntimeError::FailedToStreamResponse)?;
            let derived_events = pending.apply(event)?;
            self.agent.publish_pending_turn(&pending);

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
        self.agent.clear_pending_turn();
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
        self.push_history(Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: format_inbox(&messages),
            }],
        });
        Ok(())
    }

    pub(super) fn clear_inflight_team_messages(&mut self) {
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
        self.push_history(Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: format_background_results(&notifications),
            }],
        });
    }

    pub(super) fn clear_inflight_background_notifications(&mut self) {
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
