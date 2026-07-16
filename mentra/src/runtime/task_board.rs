use std::path::PathBuf;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

use super::{RuntimeHandle, TaskItem, TaskStatus, task::TaskAccess};
use crate::runtime::task::TaskIntrinsicTool;

/// Input for creating a persisted task through [`TaskBoard`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewTask {
    pub subject: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<u64>,
}

impl NewTask {
    pub fn new(subject: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            description: String::new(),
            owner: String::new(),
            working_directory: None,
            blocked_by: Vec::new(),
        }
    }
}

/// Typed fields accepted by [`TaskBoard::update`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::task::deserialize_present_nullable_string"
    )]
    pub working_directory: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<TaskStatus>,
}

/// Error returned by typed task-board operations.
#[derive(Debug, Error)]
pub enum TaskBoardError {
    #[error("{0}")]
    Operation(String),
    #[error("task operation returned an incompatible result: {0}")]
    InvalidResult(#[source] serde_json::Error),
}

#[derive(Debug, Clone)]
enum BoardAccess {
    Lead,
    Teammate(String),
}

/// Cloneable typed façade over Mentra's dependency-aware task board.
///
/// The façade deliberately delegates every operation to the builtin task
/// executor, preserving one implementation of access checks, DAG validation,
/// status propagation, and storage transactions. A `TaskBoard` never caches
/// task items: every read observes the store when the method is called.
#[derive(Clone)]
pub struct TaskBoard {
    runtime: RuntimeHandle,
    namespace: PathBuf,
    access: BoardAccess,
}

impl TaskBoard {
    pub(crate) fn lead(runtime: RuntimeHandle, namespace: PathBuf) -> Self {
        Self {
            runtime,
            namespace,
            access: BoardAccess::Lead,
        }
    }

    pub(crate) fn agent(
        runtime: RuntimeHandle,
        namespace: PathBuf,
        name: String,
        is_teammate: bool,
    ) -> Self {
        Self {
            runtime,
            namespace,
            access: if is_teammate {
                BoardAccess::Teammate(name)
            } else {
                BoardAccess::Lead
            },
        }
    }

    pub fn create(&self, spec: NewTask) -> Result<TaskItem, TaskBoardError> {
        self.execute(TaskIntrinsicTool::Create, spec, self.access())
    }

    pub fn get(&self, id: u64) -> Result<TaskItem, TaskBoardError> {
        self.execute(
            TaskIntrinsicTool::Get,
            serde_json::json!({ "taskId": id }),
            self.access(),
        )
    }

    pub fn list(&self) -> Result<Vec<TaskItem>, TaskBoardError> {
        let result: TaskListResult = self.execute(
            TaskIntrinsicTool::List,
            serde_json::json!({}),
            self.access(),
        )?;
        Ok(result.tasks)
    }

    pub fn update(&self, id: u64, patch: TaskPatch) -> Result<TaskItem, TaskBoardError> {
        let mut input = serde_json::to_value(patch).map_err(TaskBoardError::InvalidResult)?;
        input
            .as_object_mut()
            .expect("TaskPatch serializes as an object")
            .insert("taskId".to_string(), serde_json::json!(id));
        let result: TaskUpdateResult =
            self.execute(TaskIntrinsicTool::Update, input, self.access())?;
        Ok(result.task)
    }

    /// Claims a ready task for `owner`.
    ///
    /// Runtime-scoped boards have lead access and therefore require an
    /// explicit claimant instead of pretending the host is a teammate.
    /// Teammate-scoped boards reject any owner other than the agent itself.
    pub fn claim(&self, id: Option<u64>, owner: &str) -> Result<TaskItem, TaskBoardError> {
        let owner = owner.trim();
        if owner.is_empty() {
            return Err(TaskBoardError::Operation(
                "Task claimant must not be empty".to_string(),
            ));
        }

        let access = match &self.access {
            BoardAccess::Lead => TaskAccess::LeadClaimant(owner),
            BoardAccess::Teammate(name) if name == owner => TaskAccess::Teammate(name),
            BoardAccess::Teammate(name) => {
                return Err(TaskBoardError::Operation(format!(
                    "Teammate '{name}' cannot claim a task for '{owner}'"
                )));
            }
        };
        self.execute(
            TaskIntrinsicTool::Claim,
            serde_json::json!({ "taskId": id }),
            access,
        )
    }

    pub fn add_dependency(&self, blocker: u64, dependent: u64) -> Result<TaskItem, TaskBoardError> {
        self.update_dependency(dependent, "addBlockedBy", blocker)
    }

    pub fn remove_dependency(
        &self,
        blocker: u64,
        dependent: u64,
    ) -> Result<TaskItem, TaskBoardError> {
        self.update_dependency(dependent, "removeBlockedBy", blocker)
    }

    fn update_dependency(
        &self,
        task_id: u64,
        field: &str,
        related_id: u64,
    ) -> Result<TaskItem, TaskBoardError> {
        let result: TaskUpdateResult = self.execute(
            TaskIntrinsicTool::Update,
            serde_json::json!({ "taskId": task_id, (field): [related_id] }),
            self.access(),
        )?;
        Ok(result.task)
    }

    fn access(&self) -> TaskAccess<'_> {
        match &self.access {
            BoardAccess::Lead => TaskAccess::Lead,
            BoardAccess::Teammate(name) => TaskAccess::Teammate(name),
        }
    }

    fn execute<T: DeserializeOwned>(
        &self,
        tool: TaskIntrinsicTool,
        input: impl Serialize,
        access: TaskAccess<'_>,
    ) -> Result<T, TaskBoardError> {
        let input = serde_json::to_value(input).map_err(TaskBoardError::InvalidResult)?;
        let output = self
            .runtime
            .execute_task_mutation(&tool, input, &self.namespace, access)
            .map_err(TaskBoardError::Operation)?;
        serde_json::from_str(&output).map_err(TaskBoardError::InvalidResult)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskListResult {
    tasks: Vec<TaskItem>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskUpdateResult {
    task: TaskItem,
}
