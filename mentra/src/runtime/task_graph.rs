mod graph;
mod input;
mod render;
mod store;
#[cfg(test)]
mod tests;
mod types;

use std::{fmt, io, path::Path};

use serde_json::Value;

pub(crate) const TASK_CREATE_TOOL_NAME: &str = "task_create";
pub(crate) const TASK_UPDATE_TOOL_NAME: &str = "task_update";
pub(crate) const TASK_LIST_TOOL_NAME: &str = "task_list";
pub(crate) const TASK_GET_TOOL_NAME: &str = "task_get";
pub(crate) const TASK_REMINDER_TEXT: &str = "Reminder: use task_create, task_update, task_list, or task_get only for persisted project-task tracking. Do not use task graph tools to manage persistent teammates or team protocol flows.";

pub(crate) use graph::has_unfinished_tasks;
pub(crate) use store::{TaskDiskState, TaskStore};
pub use types::{TaskItem, TaskStatus};

#[derive(Debug)]
pub(crate) enum TaskGraphError {
    Io(io::Error),
    Serde(serde_json::Error),
    Validation(String),
}

impl fmt::Display for TaskGraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "Task graph I/O failed: {error}"),
            Self::Serde(error) => write!(f, "Task graph serialization failed: {error}"),
            Self::Validation(message) => f.write_str(message),
        }
    }
}

impl From<io::Error> for TaskGraphError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for TaskGraphError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serde(value)
    }
}

pub(crate) fn is_task_graph_tool(name: &str) -> bool {
    matches!(
        name,
        TASK_CREATE_TOOL_NAME | TASK_UPDATE_TOOL_NAME | TASK_LIST_TOOL_NAME | TASK_GET_TOOL_NAME
    )
}

pub(crate) fn execute(tool_name: &str, input: Value, dir: &Path) -> Result<String, String> {
    let store = TaskStore::new(dir.to_path_buf());
    match tool_name {
        TASK_CREATE_TOOL_NAME => input::parse_task_create_input(input)
            .and_then(|parsed| store.create(parsed).map_err(|error| error.to_string())),
        TASK_UPDATE_TOOL_NAME => input::parse_task_update_input(input)
            .and_then(|parsed| store.update(parsed).map_err(|error| error.to_string())),
        TASK_GET_TOOL_NAME => input::parse_task_get_input(input)
            .and_then(|parsed| store.get(parsed.task_id).map_err(|error| error.to_string())),
        TASK_LIST_TOOL_NAME => input::parse_task_list_input(input)
            .and_then(|()| store.list().map_err(|error| error.to_string())),
        _ => Err(format!("Tool '{tool_name}' is not a task graph tool")),
    }
}
