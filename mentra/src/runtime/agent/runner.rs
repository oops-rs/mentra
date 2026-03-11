use std::borrow::Cow;

use crate::{
    provider::model::{Message, Request, Role},
    runtime::{self, TASK_TOOL_NAME, TODO_TOOL_NAME, error::RuntimeError},
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
                self.agent.note_round_without_todo();
                return Ok(());
            }

            let mut tool_results = Vec::new();
            let mut successful_todo = false;
            for call in tool_calls {
                self.agent.set_status(AgentStatus::ExecutingTool {
                    id: call.id.clone(),
                    name: call.name.clone(),
                });
                self.agent
                    .emit_event(AgentEvent::ToolExecutionStarted { call: call.clone() });

                let (result, todo_succeeded) = if !self.agent.can_use_tool(&call.name) {
                    (self.unavailable_tool_result(call), false)
                } else if call.name == TODO_TOOL_NAME {
                    (self.execute_todo(call), true)
                } else if call.name == TASK_TOOL_NAME {
                    (self.execute_task(call).await, false)
                } else {
                    (self.agent.runtime.execute_tool(call).await, false)
                };
                self.agent.emit_event(AgentEvent::ToolExecutionFinished {
                    result: result.clone(),
                });
                successful_todo |= todo_succeeded;
                tool_results.push(result);
            }

            self.agent.push_history(Message {
                role: Role::User,
                content: tool_results,
            });
            if !successful_todo {
                self.agent.note_round_without_todo();
            }
            self.agent.clear_pending_turn();
        }
    }

    async fn stream_turn(&mut self) -> Result<PendingAssistantTurn, RuntimeError> {
        self.agent.set_status(AgentStatus::AwaitingModel);
        let provider = self.agent.provider.clone();
        let tools = self.agent.tools();
        let mut stream = {
            let request = Request {
                model: self.agent.model.as_str().into(),
                system: self.agent.effective_system_prompt(),
                messages: self.agent.history.as_slice().into(),
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

    fn execute_todo(
        &mut self,
        call: crate::tool::ToolCall,
    ) -> crate::provider::model::ContentBlock {
        match runtime::todo::parse_todo_input(call.input) {
            Ok(items) => {
                let rendered = runtime::todo::render_todos(&items);
                self.agent.apply_todo_items(items);
                crate::provider::model::ContentBlock::ToolResult {
                    tool_use_id: call.id,
                    content: rendered,
                    is_error: false,
                }
            }
            Err(content) => crate::provider::model::ContentBlock::ToolResult {
                tool_use_id: call.id,
                content,
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
                let mut child = self.agent.spawn_subagent();
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
