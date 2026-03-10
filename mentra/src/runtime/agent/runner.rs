use crate::{
    provider::model::{Message, Request, Role},
    runtime::{self, TODO_TOOL_NAME, error::RuntimeError},
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
        loop {
            let request = Request {
                model: self.agent.model.clone(),
                system: self.agent.effective_system_prompt(),
                messages: self.agent.history.clone(),
                tools: self.agent.runtime.tools(),
                tool_choice: self.agent.config.tool_choice.clone(),
                temperature: self.agent.config.temperature,
                max_output_tokens: self.agent.config.max_output_tokens,
                metadata: self.agent.config.metadata.clone(),
            };

            let pending = self.stream_turn(request).await?;
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

                let (result, todo_succeeded) = if call.name == TODO_TOOL_NAME {
                    self.execute_todo(call)
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

    async fn stream_turn(
        &mut self,
        request: Request,
    ) -> Result<PendingAssistantTurn, RuntimeError> {
        self.agent.set_status(AgentStatus::AwaitingModel);
        let mut stream = self
            .agent
            .provider
            .stream(request)
            .await
            .map_err(RuntimeError::FailedToStreamResponse)?;

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
    ) -> (crate::provider::model::ContentBlock, bool) {
        match runtime::todo::parse_todo_input(call.input) {
            Ok(items) => {
                let rendered = runtime::todo::render_todos(&items);
                self.agent.apply_todo_items(items);
                (
                    crate::provider::model::ContentBlock::ToolResult {
                        tool_use_id: call.id,
                        content: rendered,
                        is_error: false,
                    },
                    true,
                )
            }
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
}
