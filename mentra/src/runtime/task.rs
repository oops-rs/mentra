use std::borrow::Cow;

use serde::Deserialize;
use serde_json::Value;

pub(crate) const TASK_TOOL_NAME: &str = "task";
pub(crate) const SUBAGENT_MAX_ROUNDS: usize = 30;
const SUBAGENT_SYSTEM_PROMPT: &str = "You are a subagent working for another agent. Solve the delegated task, use tools when helpful, and finish with a concise final answer for the parent agent.";

#[derive(Debug, Deserialize)]
struct TaskInput {
    prompt: String,
}

pub(crate) fn parse_task_input(input: Value) -> Result<String, String> {
    let parsed = serde_json::from_value::<TaskInput>(input)
        .map_err(|error| format!("Invalid task input: {error}"))?;

    if parsed.prompt.trim().is_empty() {
        return Err("Task prompt must not be empty".to_string());
    }

    Ok(parsed.prompt)
}

pub(crate) fn build_subagent_system_prompt(base: Option<Cow<'_, str>>) -> String {
    match base {
        Some(system) => format!("{system}\n\n{SUBAGENT_SYSTEM_PROMPT}"),
        None => SUBAGENT_SYSTEM_PROMPT.to_string(),
    }
}
