use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    ContentBlock, Message, Role,
    error::RuntimeError,
    runtime::RunOptions,
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
    /// Whether the run keeps its ordinary tools while it answers.
    ///
    /// `false` — what [`new`](Self::new) gives you — is a *shaping* turn: the
    /// generated terminal tool is the only tool the run holds, so it can only
    /// put a shape on what the conversation already contains. `true` — see
    /// [`with_tools`](Self::with_tools) — is a *working* turn: the run keeps
    /// the agent's whole toolset and ends by calling the terminal tool.
    /// [`Agent::run_to_output`] describes what each costs.
    pub keeps_tools: bool,
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
            keeps_tools: false,
        }
    }

    /// Lets the run work before it answers, instead of only shaping what it
    /// already has.
    ///
    /// A shaping turn cannot read a file, run a command, or reach an MCP
    /// server, so asking one for anything it has not already been told
    /// produces a well-formed answer from a model that looked at nothing —
    /// and reports it as a success. The way out has been to spend two turns
    /// on every read-then-answer workflow: one to gather, one to shape. This
    /// spends one. The run holds its ordinary tools alongside the terminal
    /// tool, works as many rounds as it needs, and ends the turn by calling
    /// the terminal tool with the answer.
    ///
    /// The cost is that nothing forces the ending: see
    /// [`Agent::run_to_output`] for what a run that never calls the tool
    /// returns instead.
    pub fn with_tools(mut self) -> Self {
        self.keeps_tools = true;
        self
    }

    /// Reserve this output's generated scoped tool before driving the run.
    ///
    /// Reservation has no runtime side effect. It fixes the exact provider
    /// tool name so a host can recognize protocol events and mention the tool
    /// in corrective guidance before [`Agent::run_to_reserved_output`] consumes
    /// it.
    pub fn reserve(self) -> TerminalOutputReservation {
        TerminalOutputReservation {
            tool_name: unique_tool_name(&self.tool_name),
            description: self.description,
            schema: self.schema,
            keeps_tools: self.keeps_tools,
        }
    }
}

/// One generated output tool reserved for exactly one future run.
#[derive(Debug)]
pub struct TerminalOutputReservation {
    tool_name: String,
    description: String,
    schema: Value,
    keeps_tools: bool,
}

impl TerminalOutputReservation {
    /// The exact generated name the provider will see.
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }
}

/// A host's pre-termination decision over one candidate output value.
#[derive(Debug, Clone, PartialEq)]
pub enum TerminalOutputDecision {
    /// Commit this (possibly normalized) value and end the run.
    Accept(Value),
    /// Return a provider-visible tool error and let the same run correct it.
    Reject(String),
}

type TerminalOutputValidator = dyn Fn(&Value) -> TerminalOutputDecision + Send + Sync;

/// What an in-flight [`Agent::run_to_output`] tells the rest of the agent
/// about the turn it is running: which generated tool ends it, and whether
/// the ordinary toolset is on the request beside that tool.
///
/// Read on every round by [`Agent::tools`] and [`Agent::tool_choice`], which
/// is why it holds the mode rather than the name alone — the two answers have
/// to agree about which turn this is, and a name cannot say.
#[derive(Debug, Clone)]
pub(super) struct TerminalToolGate {
    pub(super) registration: crate::tool::ToolRegistration,
    pub(super) keeps_tools: bool,
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
    /// The helper does not use provider-level `response_format`. It registers
    /// one tool whose input schema *is* the requested shape, preserves the
    /// tool input as transcript `details`, and extracts it by the exact
    /// `tool_use_id` from the newly committed final transcript item.
    ///
    /// What the run may do on its way to that call is
    /// [`TerminalOutputSpec::keeps_tools`]:
    ///
    /// - **Shaping**, the default. The terminal tool is the only tool on the
    ///   request and the provider is told to call it. The turn cannot read a
    ///   file, run a command, or reach an MCP server, so the only thing left
    ///   to decide is the shape of what the conversation already holds, and
    ///   one round decides it.
    /// - **Working**, [`TerminalOutputSpec::with_tools`]. The agent's ordinary
    ///   toolset is on the request beside the terminal tool and no choice is
    ///   forced — forcing one would preclude the very rounds that are the
    ///   point. The run gathers for as many rounds as it needs and ends the
    ///   turn by calling the terminal tool.
    ///
    /// Either way the terminal call ends the round it appears in: calls
    /// scheduled after it in that same round are never executed, and each is
    /// given an explicit `is_error` result saying so. Where the model emits
    /// two terminal calls in one round, the first is the answer and the second
    /// is one of those skipped calls.
    ///
    /// Only that call produces a value. A working run that ends any other way
    /// — on prose, or at the round boundary where [`RunOptions::stop`] or
    /// [`RunOptions::token_budget`] refuses another round — has nothing to
    /// return and fails with `MalformedProviderEvent("run completed without
    /// invoking the expected terminal tool")`, while keeping everything it
    /// gathered in the transcript. [`RunOptions::ended_early`] says which
    /// bound, when one was the reason.
    ///
    /// A run that ends on a terminal call ends on a user-role tool result, so
    /// `Agent::run` reports [`RuntimeError::EmptyAssistantResponse`] for the
    /// missing assistant message. That is bookkeeping about the wrong
    /// question here, and this helper answers the right one instead: with the
    /// expected new detail present the run succeeded, and without it the run
    /// is reported as the missing terminal call it was.
    pub async fn run_to_output<T: DeserializeOwned>(
        &mut self,
        content: impl Into<Vec<ContentBlock>>,
        options: RunOptions,
        spec: TerminalOutputSpec,
    ) -> Result<FinalOutput<T>, RuntimeError> {
        let tool_name = unique_tool_name(&spec.tool_name);
        let keeps_tools = spec.keeps_tools;
        let terminal_tool = TerminalOutputTool {
            name: tool_name.clone(),
            description: spec.description,
            schema: spec.schema,
            agent_id: self.id.clone(),
            validator: None,
            accepted_call_id: None,
        };
        let registration = self.runtime.register_agent_tool(&self.id, terminal_tool);
        *self
            .terminal_tool_gate
            .lock()
            .expect("terminal tool gate poisoned") = Some(TerminalToolGate {
            registration: registration.registration().clone(),
            keeps_tools,
        });
        let _guard = TerminalToolGuard {
            registration,
            gate: Arc::clone(&self.terminal_tool_gate),
        };

        let run_result = self.run(content, options).await;
        let terminal_result = self.terminal_result(&tool_name);

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
            (Ok(_) | Err(RuntimeError::EmptyAssistantResponse), None) => {
                Err(RuntimeError::MalformedProviderEvent(
                    "run completed without invoking the expected terminal tool".to_string(),
                ))
            }
            (Err(error), _) => Err(error),
        }
    }

    /// Runs to a reserved output whose candidate is validated before the
    /// generated tool may terminate the run.
    ///
    /// A rejection is an ordinary tool error visible to the model; the same
    /// run continues with its transcript and bounds intact. Acceptance may
    /// normalize the raw input by returning a different JSON value. Existing
    /// [`run_to_output`](Self::run_to_output) semantics remain unchanged.
    pub async fn run_to_reserved_output<T, V>(
        &mut self,
        content: impl Into<Vec<ContentBlock>>,
        options: RunOptions,
        reservation: TerminalOutputReservation,
        validator: V,
    ) -> Result<FinalOutput<T>, RuntimeError>
    where
        T: DeserializeOwned,
        V: Fn(&Value) -> TerminalOutputDecision + Send + Sync + 'static,
    {
        let TerminalOutputReservation {
            tool_name,
            description,
            schema,
            keeps_tools,
        } = reservation;
        let accepted_call_id = Arc::new(Mutex::new(None));
        let terminal_tool = TerminalOutputTool {
            name: tool_name.clone(),
            description,
            schema,
            agent_id: self.id.clone(),
            validator: Some(Arc::new(validator)),
            accepted_call_id: Some(Arc::clone(&accepted_call_id)),
        };
        let registration = self.runtime.register_agent_tool(&self.id, terminal_tool);
        *self
            .terminal_tool_gate
            .lock()
            .expect("terminal tool gate poisoned") = Some(TerminalToolGate {
            registration: registration.registration().clone(),
            keeps_tools,
        });
        let _guard = TerminalToolGuard {
            registration,
            gate: Arc::clone(&self.terminal_tool_gate),
        };

        let run_result = self.run(content, options).await;
        let accepted = accepted_call_id
            .lock()
            .expect("accepted terminal call poisoned")
            .clone();
        let terminal_result = accepted
            .as_deref()
            .and_then(|call_id| self.terminal_result_for_call(&tool_name, call_id));

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
            (Ok(_) | Err(RuntimeError::EmptyAssistantResponse), None) => {
                Err(RuntimeError::MalformedProviderEvent(
                    "run completed without an accepted terminal output".to_string(),
                ))
            }
            (Err(error), _) => Err(error),
        }
    }

    fn terminal_result(&self, tool_name: &str) -> Option<(Value, Message)> {
        // Generated names include a per-call timestamp and counter, so scanning
        // the whole transcript remains stale-safe even if auto-compaction
        // replaced earlier items and changed every numeric index during the run.
        let items = self.transcript().items();
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
            if expected_ids.contains(tool_use_id)
                && let Some(details) = last.detail(tool_use_id)
            {
                return Some((details.clone(), message));
            }
        }
        None
    }

    fn terminal_result_for_call(
        &self,
        tool_name: &str,
        accepted_call_id: &str,
    ) -> Option<(Value, Message)> {
        let items = self.transcript().items();
        let was_expected_call = items
            .iter()
            .filter_map(|item| item.message.as_ref())
            .filter(|message| message.role == Role::Assistant)
            .flat_map(|message| message.content.iter())
            .any(|block| {
                matches!(block, ContentBlock::ToolUse { id, name, .. }
                    if id == accepted_call_id && name == tool_name)
            });
        if !was_expected_call {
            return None;
        }
        let last = items.last()?;
        let message = last.message.clone()?;
        let has_result = message.content.iter().any(|block| {
            matches!(block, ContentBlock::ToolResult { tool_use_id, .. }
                if tool_use_id == accepted_call_id)
        });
        has_result
            .then(|| last.detail(accepted_call_id).cloned())
            .flatten()
            .map(|details| (details, message))
    }
}

struct TerminalOutputTool {
    name: String,
    description: String,
    schema: Value,
    agent_id: String,
    validator: Option<Arc<TerminalOutputValidator>>,
    accepted_call_id: Option<Arc<Mutex<Option<String>>>>,
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
        let accepted = match &self.validator {
            Some(validator) => match validator(&input) {
                TerminalOutputDecision::Accept(value) => value,
                TerminalOutputDecision::Reject(reason) => return Err(reason),
            },
            None => input,
        };
        if let Some(call_id) = &self.accepted_call_id {
            *call_id.lock().expect("accepted terminal call poisoned") =
                Some(ctx.tool_call_id.clone());
        }
        Ok(ToolOutput::structured(accepted.clone())
            .with_details(accepted)
            .terminating())
    }
}

struct TerminalToolGuard {
    registration: crate::tool::AgentToolRegistration,
    gate: Arc<Mutex<Option<TerminalToolGate>>>,
}

impl Drop for TerminalToolGuard {
    fn drop(&mut self) {
        let mut gate = self.gate.lock().expect("terminal tool gate poisoned");
        if gate.as_ref().is_some_and(|open| {
            open.registration
                .is_same_registration(self.registration.registration())
        }) {
            *gate = None;
        }
        drop(gate);
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
        .take(14)
        .collect::<String>();
    if base.is_empty() {
        base = "output".to_string();
    }
    let id = NEXT_TERMINAL_TOOL_ID.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    format!("mentra_terminal_{base}_{timestamp:016x}_{id:016x}")
}

#[cfg(test)]
mod tests {
    use super::unique_tool_name;

    #[test]
    fn generated_tool_names_fit_common_provider_limits() {
        let name = unique_tool_name("a name with punctuation and far too many characters");
        assert!(name.len() <= 64);
        assert!(
            name.chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
        );
    }
}
