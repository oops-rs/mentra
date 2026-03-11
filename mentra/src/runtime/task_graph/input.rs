use serde::Deserialize;
use serde_json::Value;

use super::types::TaskStatus;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TaskCreateInput {
    pub(crate) subject: String,
    #[serde(default)]
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) owner: String,
    #[serde(default)]
    pub(crate) blocked_by: Vec<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TaskUpdateInput {
    pub(crate) task_id: u64,
    #[serde(default)]
    pub(crate) subject: Option<String>,
    #[serde(default)]
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) owner: Option<String>,
    #[serde(default)]
    pub(crate) status: Option<TaskStatus>,
    #[serde(default)]
    pub(crate) add_blocked_by: Vec<u64>,
    #[serde(default)]
    pub(crate) remove_blocked_by: Vec<u64>,
    #[serde(default)]
    pub(crate) add_blocks: Vec<u64>,
    #[serde(default)]
    pub(crate) remove_blocks: Vec<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TaskGetInput {
    pub(crate) task_id: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TaskListInput {}

pub(crate) fn parse_task_create_input(input: Value) -> Result<TaskCreateInput, String> {
    let parsed = serde_json::from_value::<TaskCreateInput>(input)
        .map_err(|error| format!("Invalid task_create input: {error}"))?;

    if parsed.subject.trim().is_empty() {
        return Err("Task subject must not be empty".to_string());
    }

    Ok(parsed)
}

pub(crate) fn parse_task_update_input(input: Value) -> Result<TaskUpdateInput, String> {
    let parsed = serde_json::from_value::<TaskUpdateInput>(input)
        .map_err(|error| format!("Invalid task_update input: {error}"))?;

    if matches!(parsed.subject.as_deref(), Some(subject) if subject.trim().is_empty()) {
        return Err("Task subject must not be empty".to_string());
    }

    Ok(parsed)
}

pub(crate) fn parse_task_get_input(input: Value) -> Result<TaskGetInput, String> {
    serde_json::from_value::<TaskGetInput>(input)
        .map_err(|error| format!("Invalid task_get input: {error}"))
}

pub(crate) fn parse_task_list_input(input: Value) -> Result<(), String> {
    serde_json::from_value::<TaskListInput>(input)
        .map(|_| ())
        .map_err(|error| format!("Invalid task_list input: {error}"))
}
