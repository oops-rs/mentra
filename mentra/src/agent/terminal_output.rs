use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    ContentBlock, Message, Role,
    error::RuntimeError,
    runtime::{RunOptions, RuntimeHandle},
    tool::{
        ToolContext, ToolDefinition, ToolDurability, ToolExecutor, ToolOutput, ToolSideEffectLevel,
        ToolSpec,
    },
};

use super::Agent;

static NEXT_TERMINAL_TOOL_ID: AtomicU64 = AtomicU64::new(1);

/// Provider-facing definition of a typed terminal tool.
#[derive(Debug, Clone)]
pub struct TerminalOutputSpec {
    pub tool_name: String,
    pub description: String,
    pub schema: Value,
}

impl TerminalOutputSpec {
    pub fn new(
        tool_name: impl Into<String>,
        description: impl Into<String>,
        schema: Value,
    ) -> Self {
        Self {
            tool_name: tool_name.into(),
            description: description.into(),
            schema,
        }
    }
}

/// Typed value and committed tool-result message produced by [`Agent::run_to_output`].
#[derive(Debug, Clone)]
pub struct FinalOutput<T> {
    pub value: T,
    pub message: Message,
}

impl Agent {
    /// Runs until a generated, agent-scoped terminal tool returns a typed value.
    ///
    /// The helper does not use provider-level `response_format`. It exposes only
    /// one forced terminal tool during this run, preserves the tool input as
    /// transcript `details`, and extracts it by the exact `tool_use_id` from the
    /// newly committed final transcript item. Internally a terminal run commits
    /// successfully but `Agent::run` reports [`RuntimeError::EmptyAssistantResponse`]
    /// because the final message is a user-role tool result; this helper accepts
    /// that error only when the expected new detail is present.
    pub async fn run_to_output<T: DeserializeOwned>(
        &mut self,
        content: impl Into<Vec<ContentBlock>>,
        options: RunOptions,
        spec: TerminalOutputSpec,
    ) -> Result<FinalOutput<T>, RuntimeError> {
        let tool_name = unique_tool_name(&spec.tool_name);
        let terminal_tool = TerminalOutputTool {
            name: tool_name.clone(),
            description: spec.description,
            schema: spec.schema,
            agent_id: self.id.clone(),
        };
        self.runtime.register_scoped_tool(&self.id, terminal_tool);
        *self
            .terminal_tool_gate
            .lock()
            .expect("terminal tool gate poisoned") = Some(tool_name.clone());
        let _guard = TerminalToolGuard {
            runtime: self.runtime.clone(),
            agent_id: self.id.clone(),
            tool_name: tool_name.clone(),
            gate: Arc::clone(&self.terminal_tool_gate),
        };

        let transcript_start = self.transcript().len();
        let run_result = self.run(content, options).await;
        let terminal_result = self.terminal_result_since(transcript_start, &tool_name);

        match (run_result, terminal_result) {
            (Ok(_), Some((details, message)))
            | (Err(RuntimeError::EmptyAssistantResponse), Some((details, message))) => {
                let value = serde_json::from_value(details).map_err(|error| {
                    RuntimeError::MalformedProviderEvent(format!(
                        "terminal output did not match the requested type: {error}"
                    ))
                })?;
                Ok(FinalOutput { value, message })
            }
            (Err(error), _) => Err(error),
            (Ok(_), None) => Err(RuntimeError::MalformedProviderEvent(
                "run completed without invoking the expected terminal tool".to_string(),
            )),
        }
    }

    fn terminal_result_since(
        &self,
        transcript_start: usize,
        tool_name: &str,
    ) -> Option<(Value, Message)> {
        let items = self.transcript().items().get(transcript_start..)?;
        let expected_ids = items
            .iter()
            .filter_map(|item| item.message.as_ref())
            .filter(|message| message.role == Role::Assistant)
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, name, .. } if name == tool_name => Some(id.clone()),
                _ => None,
            })
            .collect::<HashSet<_>>();
        let last = items.last()?;
        let message = last.message.clone()?;
        let result_ids = message.content.iter().filter_map(|block| match block {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id),
            _ => None,
        });

        for tool_use_id in result_ids {
            if expected_ids.contains(tool_use_id) {
                if let Some(details) = last.detail(tool_use_id) {
                    return Some((details.clone(), message));
                }
            }
        }
        None
    }
}

struct TerminalOutputTool {
    name: String,
    description: String,
    schema: Value,
    agent_id: String,
}

impl ToolDefinition for TerminalOutputTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder(self.name.clone())
            .description(self.description.clone())
            .input_schema(self.schema.clone())
            .side_effect_level(ToolSideEffectLevel::None)
            .durability(ToolDurability::ReplaySafe)
            .terminal()
            .build()
    }
}

#[async_trait]
impl ToolExecutor for TerminalOutputTool {
    async fn execute_mut_output(
        &self,
        ctx: ToolContext<'_>,
        input: Value,
    ) -> Result<ToolOutput, String> {
        if ctx.agent_id != self.agent_id {
            return Err("terminal tool belongs to a different agent".to_string());
        }
        Ok(ToolOutput::structured(input.clone())
            .with_details(input)
            .terminating())
    }
}

struct TerminalToolGuard {
    runtime: RuntimeHandle,
    agent_id: String,
    tool_name: String,
    gate: Arc<Mutex<Option<String>>>,
}

impl Drop for TerminalToolGuard {
    fn drop(&mut self) {
        let mut gate = self.gate.lock().expect("terminal tool gate poisoned");
        if gate.as_deref() == Some(self.tool_name.as_str()) {
            *gate = None;
        }
        drop(gate);
        self.runtime
            .unregister_scoped_tool(&self.agent_id, &self.tool_name);
    }
}

fn unique_tool_name(base: &str) -> String {
    let mut base = base
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .take(32)
        .collect::<String>();
    if base.is_empty() {
        base = "output".to_string();
    }
    let id = NEXT_TERMINAL_TOOL_ID.fetch_add(1, Ordering::Relaxed);
    format!("mentra_terminal_{base}_{id}")
}
