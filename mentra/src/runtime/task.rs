mod graph;
mod input;
mod intrinsic;
mod render;
mod store;
#[cfg(test)]
mod tests;
mod types;

use std::{io, path::Path};

use serde_json::Value;
use thiserror::Error;

use crate::runtime::store::TaskStore;

pub(crate) use intrinsic::TaskIntrinsicTool;
pub(crate) const TASK_REMINDER_TEXT: &str = "Reminder: use task_create, task_claim, task_update, task_list, or task_get only for persisted project-task tracking. Do not use task tools to manage persistent teammates or team protocol flows.";

pub(crate) use graph::has_unfinished_tasks;
pub use types::{TaskItem, TaskStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskAccess<'a> {
    Lead,
    Teammate(&'a str),
}

#[derive(Debug, Error)]
pub(crate) enum TaskError {
    #[error("Task storage I/O failed: {0}")]
    Io(#[from] io::Error),

    #[error("Task serialization failed: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("Task validation failed: {0}")]
    Validation(String),
}

pub(crate) fn execute_with_store(
    store: &dyn TaskStore,
    tool: &TaskIntrinsicTool,
    input: Value,
    namespace: &Path,
    access: TaskAccess<'_>,
) -> Result<String, String> {
    match tool {
        TaskIntrinsicTool::Create => {
            let parsed = input::parse_task_create_input(input)?;
            let mut tasks = load_store_tasks(store, namespace)?;
            let task_id = tasks.iter().map(|task| task.id).max().unwrap_or(0) + 1;
            tasks.push(TaskItem {
                id: task_id,
                subject: parsed.subject.trim().to_string(),
                description: parsed.description,
                status: TaskStatus::Pending,
                blocked_by: Vec::new(),
                blocks: Vec::new(),
                owner: parsed.owner,
                working_directory: parsed.working_directory,
            });

            for blocker_id in parsed.blocked_by {
                graph::add_dependency(&mut tasks, blocker_id, task_id)
                    .map_err(|error| error.to_string())?;
            }

            store
                .replace_tasks(namespace, &tasks)
                .map_err(store_error)?;
            render::serialize_pretty(
                graph::find_task(&tasks, task_id).map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())
        }
        TaskIntrinsicTool::Claim => {
            let parsed = input::parse_task_claim_input(input)?;
            let owner = access
                .actor_name()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| "Only named teammates can claim tasks".to_string())?
                .trim()
                .to_string();
            let mut tasks = load_store_tasks(store, namespace)?;
            let claimed = match parsed.task_id {
                Some(task_id) => {
                    let task = store::find_task_mut(&mut tasks, task_id)
                        .map_err(|error| error.to_string())?;
                    store::validate_claimable(task, &owner).map_err(|error| error.to_string())?;
                    task.owner = owner;
                    task.clone()
                }
                None => {
                    let task = tasks
                        .iter_mut()
                        .find(|task| store::is_claimable(task))
                        .ok_or_else(|| {
                            "No ready unowned tasks are available to claim".to_string()
                        })?;
                    task.owner = owner;
                    task.clone()
                }
            };

            store
                .replace_tasks(namespace, &tasks)
                .map_err(store_error)?;
            render::serialize_pretty(&claimed).map_err(|error| error.to_string())
        }
        TaskIntrinsicTool::Update => {
            let parsed = input::parse_task_update_input(input)?;
            let mut tasks = load_store_tasks(store, namespace)?;
            let task_id = parsed.task_id;
            let original_status = graph::find_task(&tasks, task_id)
                .map_err(|error| error.to_string())?
                .status
                .clone();
            store::validate_update_access(
                graph::find_task(&tasks, task_id).map_err(|error| error.to_string())?,
                &parsed,
                access,
            )
            .map_err(|error| error.to_string())?;

            {
                let task =
                    store::find_task_mut(&mut tasks, task_id).map_err(|error| error.to_string())?;
                if let Some(subject) = parsed.subject.clone() {
                    task.subject = subject.trim().to_string();
                }
                if let Some(description) = parsed.description.clone() {
                    task.description = description;
                }
                if let Some(owner) = parsed.owner.clone() {
                    task.owner = owner;
                }
                if let Some(working_directory) = parsed.working_directory.clone() {
                    task.working_directory = working_directory;
                }
            }

            for blocker_id in parsed.add_blocked_by.clone() {
                graph::add_dependency(&mut tasks, blocker_id, task_id)
                    .map_err(|error| error.to_string())?;
            }
            for blocker_id in parsed.remove_blocked_by.clone() {
                graph::remove_dependency(&mut tasks, blocker_id, task_id)
                    .map_err(|error| error.to_string())?;
            }
            for dependent_id in parsed.add_blocks.clone() {
                graph::add_dependency(&mut tasks, task_id, dependent_id)
                    .map_err(|error| error.to_string())?;
            }
            for dependent_id in parsed.remove_blocks.clone() {
                graph::remove_dependency(&mut tasks, task_id, dependent_id)
                    .map_err(|error| error.to_string())?;
            }

            let mut unblocked = Vec::new();
            let mut reblocked = Vec::new();
            if let Some(status) = parsed.status.clone() {
                graph::apply_status_change(
                    &mut tasks,
                    task_id,
                    original_status,
                    status,
                    &mut unblocked,
                    &mut reblocked,
                )
                .map_err(|error| error.to_string())?;
            } else {
                store::validate_unblocked_status(
                    graph::find_task(&tasks, task_id).map_err(|error| error.to_string())?,
                )
                .map_err(|error| error.to_string())?;
            }

            store
                .replace_tasks(namespace, &tasks)
                .map_err(store_error)?;
            graph::sort_tasks(&mut unblocked);
            graph::sort_tasks(&mut reblocked);
            render::serialize_pretty(&render::TaskUpdateOutput {
                task: graph::find_task(&tasks, task_id)
                    .map_err(|error| error.to_string())?
                    .clone(),
                unblocked,
                reblocked,
            })
            .map_err(|error| error.to_string())
        }
        TaskIntrinsicTool::Get => {
            let parsed = input::parse_task_get_input(input)?;
            let tasks = load_store_tasks(store, namespace)?;
            render::serialize_pretty(
                graph::find_task(&tasks, parsed.task_id).map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())
        }
        TaskIntrinsicTool::List => {
            input::parse_task_list_input(input)?;
            let tasks = load_store_tasks(store, namespace)?;
            let mut ready = Vec::new();
            let mut blocked = Vec::new();
            let mut in_progress = Vec::new();
            let mut completed = Vec::new();

            for task in &tasks {
                match task.status {
                    TaskStatus::Pending if task.blocked_by.is_empty() => ready.push(task.clone()),
                    TaskStatus::Pending => blocked.push(task.clone()),
                    TaskStatus::InProgress => in_progress.push(task.clone()),
                    TaskStatus::Completed => completed.push(task.clone()),
                }
            }

            render::serialize_pretty(&render::TaskListOutput {
                tasks,
                ready,
                blocked,
                in_progress,
                completed,
            })
            .map_err(|error| error.to_string())
        }
    }
}

impl<'a> TaskAccess<'a> {
    pub(crate) fn actor_name(self) -> Option<&'a str> {
        match self {
            Self::Lead => None,
            Self::Teammate(name) => Some(name),
        }
    }
}

fn load_store_tasks(store: &dyn TaskStore, namespace: &Path) -> Result<Vec<TaskItem>, String> {
    store
        .load_tasks(namespace)
        .map_err(|error| format!("Task storage failed: {error}"))
}

fn store_error(error: crate::runtime::RuntimeError) -> String {
    format!("Task storage failed: {error}")
}
