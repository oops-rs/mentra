use serde::Serialize;

use super::TaskGraphError;
use super::types::TaskItem;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TaskUpdateOutput {
    pub(super) task: TaskItem,
    pub(super) unblocked: Vec<TaskItem>,
    pub(super) reblocked: Vec<TaskItem>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TaskListOutput {
    pub(super) tasks: Vec<TaskItem>,
    pub(super) ready: Vec<TaskItem>,
    pub(super) blocked: Vec<TaskItem>,
    pub(super) in_progress: Vec<TaskItem>,
    pub(super) completed: Vec<TaskItem>,
}

pub(super) fn serialize_pretty<T>(value: &T) -> Result<String, TaskGraphError>
where
    T: Serialize,
{
    serde_json::to_string_pretty(value).map_err(TaskGraphError::Serde)
}
