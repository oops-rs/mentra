use std::borrow::Cow;

use crate::{
    provider::model::{ContentBlock, Message, Request, Role},
    runtime::{
        self, COMPACT_TOOL_NAME, TASK_TOOL_NAME, TaskStore,
        background::BackgroundNotification,
        error::RuntimeError,
        is_task_graph_tool,
        task_graph::{
            parse_task_create_input, parse_task_get_input, parse_task_list_input,
            parse_task_update_input,
        },
    },
};

use super::{Agent, AgentEvent, AgentStatus, PendingAssistantTurn, SpawnedAgentStatus};

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
                } else if call.name == COMPACT_TOOL_NAME {
                    (self.execute_compact(call).await, false)
                } else if call.name == TASK_TOOL_NAME {
                    (self.execute_task(call).await, false)
                } else if is_task_graph_tool(&call.name) {
                    self.execute_task_graph(call)
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

    fn execute_task_graph(
        &mut self,
        call: crate::tool::ToolCall,
    ) -> (crate::provider::model::ContentBlock, bool) {
        let store = TaskStore::new(self.agent.config().task_graph.tasks_dir.clone());
        let output = match call.name.as_str() {
            runtime::TASK_CREATE_TOOL_NAME => parse_task_create_input(call.input)
                .and_then(|input| store.create(input).map_err(|error| error.to_string())),
            runtime::TASK_UPDATE_TOOL_NAME => parse_task_update_input(call.input)
                .and_then(|input| store.update(input).map_err(|error| error.to_string())),
            runtime::TASK_GET_TOOL_NAME => parse_task_get_input(call.input)
                .and_then(|input| store.get(input.task_id).map_err(|error| error.to_string())),
            runtime::TASK_LIST_TOOL_NAME => parse_task_list_input(call.input)
                .and_then(|()| store.list().map_err(|error| error.to_string())),
            _ => Err(format!("Tool '{}' is not a task graph tool", call.name)),
        };

        match output {
            Ok(content) => match self.agent.refresh_tasks_from_disk() {
                Ok(()) => (
                    crate::provider::model::ContentBlock::ToolResult {
                        tool_use_id: call.id,
                        content,
                        is_error: false,
                    },
                    true,
                ),
                Err(error) => (
                    crate::provider::model::ContentBlock::ToolResult {
                        tool_use_id: call.id,
                        content: format!("Task graph refresh failed: {error:?}"),
                        is_error: true,
                    },
                    false,
                ),
            },
            Err(content) => (
                crate::provider::model::ContentBlock::ToolResult {
                    tool_use_id: call.id,
                    content,
                    is_error: true,
                },
                false,
            ),
        }
    }

    async fn execute_compact(
        &mut self,
        call: crate::tool::ToolCall,
    ) -> crate::provider::model::ContentBlock {
        match self
            .agent
            .compact_history(
                self.agent.history.len().saturating_sub(1),
                super::ContextCompactionTrigger::Manual,
            )
            .await
        {
            Ok(Some(details)) => crate::provider::model::ContentBlock::ToolResult {
                tool_use_id: call.id,
                content: format!(
                    "Context compacted. Transcript saved to {}",
                    details.transcript_path.display()
                ),
                is_error: false,
            },
            Ok(None) => crate::provider::model::ContentBlock::ToolResult {
                tool_use_id: call.id,
                content:
                    "Context compaction skipped because there was no older history to summarize."
                        .to_string(),
                is_error: false,
            },
            Err(error) => crate::provider::model::ContentBlock::ToolResult {
                tool_use_id: call.id,
                content: format!("Context compaction failed: {error:?}"),
                is_error: true,
            },
        }
    }

    async fn execute_task(
        &mut self,
        call: crate::tool::ToolCall,
    ) -> crate::provider::model::ContentBlock {
        match runtime::task::parse_task_input(call.input) {
            Ok(prompt) => {
                let mut child = match self.agent.spawn_subagent() {
                    Ok(child) => child,
                    Err(error) => {
                        return crate::provider::model::ContentBlock::ToolResult {
                            tool_use_id: call.id,
                            content: format!("Failed to spawn subagent: {error:?}"),
                            is_error: true,
                        };
                    }
                };
                let started = self.agent.register_subagent(&child);
                self.agent
                    .emit_event(AgentEvent::SubagentSpawned { agent: started });

                match Box::pin(child.send(vec![crate::provider::model::ContentBlock::Text {
                    text: prompt,
                }]))
                .await
                {
                    Ok(()) => {
                        if let Some(finished) = self
                            .agent
                            .finish_subagent(child.id(), SpawnedAgentStatus::Finished)
                        {
                            self.agent
                                .emit_event(AgentEvent::SubagentFinished { agent: finished });
                        }
                        if let Err(error) = self.agent.refresh_tasks_from_disk() {
                            return crate::provider::model::ContentBlock::ToolResult {
                                tool_use_id: call.id,
                                content: format!("Task graph refresh failed: {error:?}"),
                                is_error: true,
                            };
                        }

                        crate::provider::model::ContentBlock::ToolResult {
                            tool_use_id: call.id,
                            content: child.final_text_summary(),
                            is_error: false,
                        }
                    }
                    Err(error) => {
                        if let Some(finished) = self.agent.finish_subagent(
                            child.id(),
                            SpawnedAgentStatus::Failed(format!("{error:?}")),
                        ) {
                            self.agent
                                .emit_event(AgentEvent::SubagentFinished { agent: finished });
                        }
                        let _ = self.agent.refresh_tasks_from_disk();

                        crate::provider::model::ContentBlock::ToolResult {
                            tool_use_id: call.id,
                            content: format!("Subagent failed: {error:?}"),
                            is_error: true,
                        }
                    }
                }
            }
            Err(content) => crate::provider::model::ContentBlock::ToolResult {
                tool_use_id: call.id,
                content,
                is_error: true,
            },
        }
    }

    fn unavailable_tool_result(
        &self,
        call: crate::tool::ToolCall,
    ) -> crate::provider::model::ContentBlock {
        crate::provider::model::ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Tool '{}' is not available for this agent", call.name),
            is_error: true,
        }
    }
}

impl Agent {
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
