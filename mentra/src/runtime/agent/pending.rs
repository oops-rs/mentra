use std::collections::BTreeMap;

use crate::{
    Message, Role,
    provider::{ContentBlockDelta, ProviderEvent},
    runtime::error::RuntimeError,
    tool::ToolCall,
};

use super::{AgentEvent, PendingToolUseSummary, pending_block::PendingContentBlock};

#[derive(Debug, Clone, Default)]
pub struct PendingAssistantTurn {
    id: Option<String>,
    model: Option<String>,
    role: Option<Role>,
    blocks: BTreeMap<usize, PendingContentBlock>,
    current_text: String,
    stop_reason: Option<String>,
    stopped: bool,
}

impl PendingAssistantTurn {
    pub fn apply(&mut self, event: ProviderEvent) -> Result<Vec<AgentEvent>, RuntimeError> {
        let mut derived_events = Vec::new();

        match event {
            ProviderEvent::MessageStarted { id, model, role } => {
                self.id = Some(id);
                self.model = Some(model);
                self.role = Some(role);
            }
            ProviderEvent::ContentBlockStarted { index, kind } => {
                self.blocks.insert(index, PendingContentBlock::from(kind));
            }
            ProviderEvent::ContentBlockDelta { index, delta } => {
                let block = self.blocks.get_mut(&index).ok_or_else(|| {
                    RuntimeError::MalformedProviderEvent(format!(
                        "content block delta received before start for index {index}"
                    ))
                })?;

                match (block, delta) {
                    (PendingContentBlock::Text { text, .. }, ContentBlockDelta::Text(delta)) => {
                        text.push_str(&delta);
                        self.current_text.push_str(&delta);
                        derived_events.push(AgentEvent::TextDelta {
                            delta,
                            full_text: self.current_text.clone(),
                        });
                    }
                    (
                        PendingContentBlock::ToolUse {
                            id,
                            name,
                            input_json,
                            ..
                        },
                        ContentBlockDelta::ToolUseInputJson(delta),
                    ) => {
                        input_json.push_str(&delta);
                        derived_events.push(AgentEvent::ToolUseUpdated {
                            index,
                            id: id.clone(),
                            name: name.clone(),
                            input_json: input_json.clone(),
                        });
                    }
                    (
                        PendingContentBlock::ToolResult { content, .. },
                        ContentBlockDelta::ToolResultContent(delta),
                    ) => content.push_str(&delta),
                    (block, delta) => {
                        return Err(RuntimeError::MalformedProviderEvent(format!(
                            "delta {delta:?} is not valid for block {}",
                            block.kind_name()
                        )));
                    }
                }
            }
            ProviderEvent::ContentBlockStopped { index } => {
                let block = self.blocks.get_mut(&index).ok_or_else(|| {
                    RuntimeError::MalformedProviderEvent(format!(
                        "content block stop received before start for index {index}"
                    ))
                })?;
                block.mark_complete();

                if let PendingContentBlock::ToolUse {
                    id,
                    name,
                    input_json,
                    ..
                } = block
                {
                    let input = serde_json::from_str(input_json).map_err(|source| {
                        RuntimeError::InvalidToolUseInput {
                            id: id.clone(),
                            name: name.clone(),
                            source,
                        }
                    })?;

                    derived_events.push(AgentEvent::ToolUseReady {
                        index,
                        call: ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            input,
                        },
                    });
                }
            }
            ProviderEvent::MessageDelta { stop_reason } => self.stop_reason = stop_reason,
            ProviderEvent::MessageStopped => self.stopped = true,
        }

        Ok(derived_events)
    }

    pub fn to_message(&self) -> Result<Message, RuntimeError> {
        if !self.stopped {
            return Err(RuntimeError::MalformedProviderEvent(
                "assistant turn ended before MessageStopped".to_string(),
            ));
        }

        let role = self.role.clone().ok_or_else(|| {
            RuntimeError::MalformedProviderEvent("assistant turn missing role".to_string())
        })?;
        let mut content = Vec::with_capacity(self.blocks.len());

        for (index, block) in &self.blocks {
            if !block.is_complete() {
                return Err(RuntimeError::MalformedProviderEvent(format!(
                    "content block {index} did not complete"
                )));
            }
            content.push(block.to_content_block()?);
        }

        Ok(Message { role, content })
    }

    pub fn ready_tool_calls(&self) -> Result<Vec<ToolCall>, RuntimeError> {
        let mut tool_calls = Vec::new();

        for block in self.blocks.values() {
            if let PendingContentBlock::ToolUse {
                id,
                name,
                input_json,
                complete,
            } = block
                && *complete
            {
                let input = serde_json::from_str(input_json).map_err(|source| {
                    RuntimeError::InvalidToolUseInput {
                        id: id.clone(),
                        name: name.clone(),
                        source,
                    }
                })?;
                tool_calls.push(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    input,
                });
            }
        }

        Ok(tool_calls)
    }

    pub fn pending_tool_use_summaries(&self) -> Vec<PendingToolUseSummary> {
        self.blocks
            .values()
            .filter_map(PendingContentBlock::tool_use_summary)
            .collect()
    }

    pub fn current_text(&self) -> &str {
        &self.current_text
    }
}
