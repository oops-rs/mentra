use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fmt, fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const TASK_CREATE_TOOL_NAME: &str = "task_create";
pub(crate) const TASK_UPDATE_TOOL_NAME: &str = "task_update";
pub(crate) const TASK_LIST_TOOL_NAME: &str = "task_list";
pub(crate) const TASK_GET_TOOL_NAME: &str = "task_get";
pub(crate) const TASK_REMINDER_TEXT: &str = "Reminder: update the task graph with task_create, task_update, task_list, or task_get before continuing multi-step work.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskItem {
    pub id: u64,
    pub subject: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub status: TaskStatus,
    #[serde(default)]
    pub blocked_by: Vec<u64>,
    #[serde(default)]
    pub blocks: Vec<u64>,
    #[serde(default)]
    pub owner: String,
}

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

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskUpdateOutput {
    task: TaskItem,
    unblocked: Vec<TaskItem>,
    reblocked: Vec<TaskItem>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskListOutput {
    tasks: Vec<TaskItem>,
    ready: Vec<TaskItem>,
    blocked: Vec<TaskItem>,
    in_progress: Vec<TaskItem>,
    completed: Vec<TaskItem>,
}

#[derive(Debug, Clone)]
pub(crate) struct TaskDiskState {
    existed: bool,
    files: Vec<(String, String)>,
}

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

pub(crate) fn has_unfinished_tasks(tasks: &[TaskItem]) -> bool {
    tasks
        .iter()
        .any(|task| task.status != TaskStatus::Completed)
}

pub(crate) fn is_task_graph_tool(name: &str) -> bool {
    matches!(
        name,
        TASK_CREATE_TOOL_NAME | TASK_UPDATE_TOOL_NAME | TASK_LIST_TOOL_NAME | TASK_GET_TOOL_NAME
    )
}

pub(crate) struct TaskStore {
    dir: PathBuf,
}

impl TaskStore {
    pub(crate) fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub(crate) fn load_all(&self) -> Result<Vec<TaskItem>, TaskGraphError> {
        if !self.dir.exists() {
            return Ok(Vec::new());
        }

        let mut tasks = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if !is_task_file(&path) {
                continue;
            }

            let content = fs::read_to_string(&path)?;
            let mut task = serde_json::from_str::<TaskItem>(&content)?;
            sort_and_dedup_ids(&mut task.blocked_by);
            sort_and_dedup_ids(&mut task.blocks);
            tasks.push(task);
        }

        tasks.sort_by_key(|task| task.id);
        validate_loaded_tasks(&tasks)?;
        Ok(tasks)
    }

    pub(crate) fn capture_disk_state(&self) -> Result<TaskDiskState, TaskGraphError> {
        if !self.dir.exists() {
            return Ok(TaskDiskState {
                existed: false,
                files: Vec::new(),
            });
        }

        let mut files = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if !is_task_file(&path) {
                continue;
            }

            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            files.push((name.to_string(), fs::read_to_string(&path)?));
        }
        files.sort_by(|left, right| left.0.cmp(&right.0));

        Ok(TaskDiskState {
            existed: true,
            files,
        })
    }

    pub(crate) fn restore_disk_state(&self, state: &TaskDiskState) -> Result<(), TaskGraphError> {
        if self.dir.exists() {
            for path in self.task_file_paths()? {
                fs::remove_file(path)?;
            }
        }

        if !state.existed && state.files.is_empty() {
            if self.dir.exists() {
                match fs::remove_dir(&self.dir) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {}
                    Err(error) => return Err(TaskGraphError::Io(error)),
                }
            }
            return Ok(());
        }

        fs::create_dir_all(&self.dir)?;
        for (name, content) in &state.files {
            self.write_raw_file(&self.dir.join(name), content)?;
        }
        Ok(())
    }

    pub(crate) fn create(&self, input: TaskCreateInput) -> Result<String, TaskGraphError> {
        let mut tasks = self.load_all()?;
        let task_id = tasks.iter().map(|task| task.id).max().unwrap_or(0) + 1;
        tasks.push(TaskItem {
            id: task_id,
            subject: input.subject.trim().to_string(),
            description: input.description,
            status: TaskStatus::Pending,
            blocked_by: Vec::new(),
            blocks: Vec::new(),
            owner: input.owner,
        });

        for blocker_id in input.blocked_by {
            add_dependency(&mut tasks, blocker_id, task_id)?;
        }

        self.write_all(&tasks)?;
        let created = find_task(&tasks, task_id)?.clone();
        serialize_pretty(&created)
    }

    pub(crate) fn update(&self, input: TaskUpdateInput) -> Result<String, TaskGraphError> {
        let mut tasks = self.load_all()?;
        let task_id = input.task_id;
        let original_status = find_task(&tasks, task_id)?.status.clone();

        {
            let task = find_task_mut(&mut tasks, task_id)?;
            if let Some(subject) = input.subject {
                task.subject = subject.trim().to_string();
            }
            if let Some(description) = input.description {
                task.description = description;
            }
            if let Some(owner) = input.owner {
                task.owner = owner;
            }
        }

        for blocker_id in input.add_blocked_by {
            add_dependency(&mut tasks, blocker_id, task_id)?;
        }
        for blocker_id in input.remove_blocked_by {
            remove_dependency(&mut tasks, blocker_id, task_id)?;
        }
        for dependent_id in input.add_blocks {
            add_dependency(&mut tasks, task_id, dependent_id)?;
        }
        for dependent_id in input.remove_blocks {
            remove_dependency(&mut tasks, task_id, dependent_id)?;
        }

        let mut unblocked = Vec::new();
        let mut reblocked = Vec::new();
        if let Some(status) = input.status {
            apply_status_change(
                &mut tasks,
                task_id,
                original_status,
                status,
                &mut unblocked,
                &mut reblocked,
            )?;
        } else {
            validate_unblocked_status(find_task(&tasks, task_id)?)?;
        }

        self.write_all(&tasks)?;
        sort_tasks(&mut unblocked);
        sort_tasks(&mut reblocked);

        serialize_pretty(&TaskUpdateOutput {
            task: find_task(&tasks, task_id)?.clone(),
            unblocked,
            reblocked,
        })
    }

    pub(crate) fn get(&self, task_id: u64) -> Result<String, TaskGraphError> {
        let tasks = self.load_all()?;
        serialize_pretty(find_task(&tasks, task_id)?)
    }

    pub(crate) fn list(&self) -> Result<String, TaskGraphError> {
        let tasks = self.load_all()?;
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

        serialize_pretty(&TaskListOutput {
            tasks,
            ready,
            blocked,
            in_progress,
            completed,
        })
    }

    fn write_all(&self, tasks: &[TaskItem]) -> Result<(), TaskGraphError> {
        if tasks.is_empty() {
            return Ok(());
        }

        fs::create_dir_all(&self.dir)?;
        for task in tasks {
            self.write_raw_file(
                &self.task_path(task.id),
                &serde_json::to_string_pretty(task)?,
            )?;
        }
        Ok(())
    }

    fn write_raw_file(&self, path: &Path, content: &str) -> Result<(), TaskGraphError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let temp_path = path.with_extension(format!("json.tmp-{unique}"));
        fs::write(&temp_path, content)?;
        fs::rename(&temp_path, path)?;
        Ok(())
    }

    fn task_path(&self, task_id: u64) -> PathBuf {
        self.dir.join(format!("task_{task_id}.json"))
    }

    fn task_file_paths(&self) -> Result<Vec<PathBuf>, TaskGraphError> {
        if !self.dir.exists() {
            return Ok(Vec::new());
        }

        let mut paths = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if is_task_file(&path) {
                paths.push(path);
            }
        }
        Ok(paths)
    }
}

fn serialize_pretty<T>(value: &T) -> Result<String, TaskGraphError>
where
    T: Serialize,
{
    serde_json::to_string_pretty(value).map_err(TaskGraphError::Serde)
}

fn apply_status_change(
    tasks: &mut [TaskItem],
    task_id: u64,
    original_status: TaskStatus,
    next_status: TaskStatus,
    unblocked: &mut Vec<TaskItem>,
    reblocked: &mut Vec<TaskItem>,
) -> Result<(), TaskGraphError> {
    if original_status == next_status {
        validate_unblocked_status(find_task(tasks, task_id)?)?;
        return Ok(());
    }

    match next_status {
        TaskStatus::Completed => {
            {
                let task = find_task_mut(tasks, task_id)?;
                validate_unblocked_status(task)?;
                task.status = TaskStatus::Completed;
            }

            let dependents = find_task(tasks, task_id)?.blocks.clone();
            for dependent_id in dependents {
                let dependent = find_task_mut(tasks, dependent_id)?;
                if dependent.status == TaskStatus::Completed {
                    continue;
                }

                let had_blocker = remove_id(&mut dependent.blocked_by, task_id);
                if had_blocker && dependent.blocked_by.is_empty() {
                    unblocked.push(dependent.clone());
                }
            }
        }
        TaskStatus::Pending | TaskStatus::InProgress => {
            {
                let task = find_task_mut(tasks, task_id)?;
                task.status = next_status.clone();
                validate_unblocked_status(task)?;
            }

            if original_status == TaskStatus::Completed {
                let dependents = find_task(tasks, task_id)?.blocks.clone();
                for dependent_id in dependents {
                    let dependent = find_task_mut(tasks, dependent_id)?;
                    if dependent.status == TaskStatus::Completed {
                        continue;
                    }

                    if insert_id(&mut dependent.blocked_by, task_id) {
                        reblocked.push(dependent.clone());
                    }
                }
            }
        }
    }

    Ok(())
}

fn add_dependency(
    tasks: &mut [TaskItem],
    blocker_id: u64,
    dependent_id: u64,
) -> Result<(), TaskGraphError> {
    if blocker_id == dependent_id {
        return Err(TaskGraphError::Validation(
            "Tasks cannot depend on themselves".to_string(),
        ));
    }

    let blocker_status = find_task(tasks, blocker_id)?.status.clone();
    let dependent_status = find_task(tasks, dependent_id)?.status.clone();

    let edge_exists = find_task(tasks, blocker_id)?.blocks.contains(&dependent_id);
    if !edge_exists && path_exists(tasks, dependent_id, blocker_id) {
        return Err(TaskGraphError::Validation(format!(
            "Adding dependency {blocker_id} -> {dependent_id} would create a cycle"
        )));
    }

    insert_id(&mut find_task_mut(tasks, blocker_id)?.blocks, dependent_id);

    if blocker_status != TaskStatus::Completed {
        if dependent_status != TaskStatus::Pending {
            return Err(TaskGraphError::Validation(format!(
                "Task {dependent_id} cannot have unresolved blockers while {:?}",
                dependent_status
            )));
        }

        insert_id(
            &mut find_task_mut(tasks, dependent_id)?.blocked_by,
            blocker_id,
        );
    }

    Ok(())
}

fn remove_dependency(
    tasks: &mut [TaskItem],
    blocker_id: u64,
    dependent_id: u64,
) -> Result<(), TaskGraphError> {
    find_task(tasks, blocker_id)?;
    find_task(tasks, dependent_id)?;

    remove_id(&mut find_task_mut(tasks, blocker_id)?.blocks, dependent_id);
    remove_id(
        &mut find_task_mut(tasks, dependent_id)?.blocked_by,
        blocker_id,
    );
    Ok(())
}

fn validate_loaded_tasks(tasks: &[TaskItem]) -> Result<(), TaskGraphError> {
    let mut seen = HashSet::new();
    let task_ids = tasks.iter().map(|task| task.id).collect::<HashSet<_>>();
    let tasks_by_id = tasks
        .iter()
        .map(|task| (task.id, task))
        .collect::<HashMap<_, _>>();

    for task in tasks {
        if !seen.insert(task.id) {
            return Err(TaskGraphError::Validation(format!(
                "Duplicate task id {} on disk",
                task.id
            )));
        }

        validate_unblocked_status(task)?;

        for blocker_id in &task.blocked_by {
            if !task_ids.contains(blocker_id) {
                return Err(TaskGraphError::Validation(format!(
                    "Task {} references missing blocker {}",
                    task.id, blocker_id
                )));
            }
            if *blocker_id == task.id {
                return Err(TaskGraphError::Validation(format!(
                    "Task {} cannot block itself",
                    task.id
                )));
            }

            let blocker = tasks_by_id[blocker_id];
            if blocker.status == TaskStatus::Completed {
                return Err(TaskGraphError::Validation(format!(
                    "Task {} is still blocked by completed task {}",
                    task.id, blocker_id
                )));
            }
            if !blocker.blocks.contains(&task.id) {
                return Err(TaskGraphError::Validation(format!(
                    "Task {} is blocked by {} but the reciprocal edge is missing",
                    task.id, blocker_id
                )));
            }
        }

        for dependent_id in &task.blocks {
            if !task_ids.contains(dependent_id) {
                return Err(TaskGraphError::Validation(format!(
                    "Task {} references missing dependent {}",
                    task.id, dependent_id
                )));
            }
            if *dependent_id == task.id {
                return Err(TaskGraphError::Validation(format!(
                    "Task {} cannot depend on itself",
                    task.id
                )));
            }

            let dependent = tasks_by_id[dependent_id];
            if task.status != TaskStatus::Completed && !dependent.blocked_by.contains(&task.id) {
                return Err(TaskGraphError::Validation(format!(
                    "Task {} blocks {} but the unresolved blocker is missing",
                    task.id, dependent_id
                )));
            }
        }
    }

    if has_cycle(tasks) {
        return Err(TaskGraphError::Validation(
            "Task graph contains a cycle".to_string(),
        ));
    }

    Ok(())
}

fn validate_unblocked_status(task: &TaskItem) -> Result<(), TaskGraphError> {
    if matches!(task.status, TaskStatus::InProgress | TaskStatus::Completed)
        && !task.blocked_by.is_empty()
    {
        return Err(TaskGraphError::Validation(format!(
            "Task {} cannot be {:?} while blocked by {:?}",
            task.id, task.status, task.blocked_by
        )));
    }

    Ok(())
}

fn find_task(tasks: &[TaskItem], task_id: u64) -> Result<&TaskItem, TaskGraphError> {
    tasks
        .iter()
        .find(|task| task.id == task_id)
        .ok_or_else(|| TaskGraphError::Validation(format!("Task {task_id} does not exist")))
}

fn find_task_mut(tasks: &mut [TaskItem], task_id: u64) -> Result<&mut TaskItem, TaskGraphError> {
    tasks
        .iter_mut()
        .find(|task| task.id == task_id)
        .ok_or_else(|| TaskGraphError::Validation(format!("Task {task_id} does not exist")))
}

fn sort_and_dedup_ids(ids: &mut Vec<u64>) {
    let unique = ids.iter().copied().collect::<BTreeSet<_>>();
    ids.clear();
    ids.extend(unique);
}

fn sort_tasks(tasks: &mut [TaskItem]) {
    tasks.sort_by_key(|task| task.id);
}

fn insert_id(ids: &mut Vec<u64>, id: u64) -> bool {
    if ids.contains(&id) {
        return false;
    }

    ids.push(id);
    sort_and_dedup_ids(ids);
    true
}

fn remove_id(ids: &mut Vec<u64>, id: u64) -> bool {
    let len_before = ids.len();
    ids.retain(|current| *current != id);
    len_before != ids.len()
}

fn path_exists(tasks: &[TaskItem], start: u64, goal: u64) -> bool {
    if start == goal {
        return true;
    }

    let tasks_by_id = tasks
        .iter()
        .map(|task| (task.id, task))
        .collect::<HashMap<_, _>>();
    let mut visited = HashSet::new();
    let mut stack = vec![start];

    while let Some(task_id) = stack.pop() {
        if !visited.insert(task_id) {
            continue;
        }

        let Some(task) = tasks_by_id.get(&task_id) else {
            continue;
        };
        for next in &task.blocks {
            if *next == goal {
                return true;
            }
            stack.push(*next);
        }
    }

    false
}

fn has_cycle(tasks: &[TaskItem]) -> bool {
    let tasks_by_id = tasks
        .iter()
        .map(|task| (task.id, task))
        .collect::<HashMap<_, _>>();
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();

    for task in tasks {
        if dfs_has_cycle(task.id, &tasks_by_id, &mut visiting, &mut visited) {
            return true;
        }
    }

    false
}

fn dfs_has_cycle(
    task_id: u64,
    tasks_by_id: &HashMap<u64, &TaskItem>,
    visiting: &mut HashSet<u64>,
    visited: &mut HashSet<u64>,
) -> bool {
    if visited.contains(&task_id) {
        return false;
    }
    if !visiting.insert(task_id) {
        return true;
    }

    if let Some(task) = tasks_by_id.get(&task_id) {
        for next in &task.blocks {
            if dfs_has_cycle(*next, tasks_by_id, visiting, visited) {
                return true;
            }
        }
    }

    visiting.remove(&task_id);
    visited.insert(task_id);
    false
}

fn is_task_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.starts_with("task_") && name.ends_with(".json"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{
        TaskCreateInput, TaskStatus, TaskStore, parse_task_create_input, parse_task_list_input,
        parse_task_update_input,
    };

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn create_and_list_group_ready_blocked_and_completed_tasks() {
        let store = TaskStore::new(temp_dir("grouping"));

        store
            .create(TaskCreateInput {
                subject: "Plan".to_string(),
                description: String::new(),
                owner: String::new(),
                blocked_by: Vec::new(),
            })
            .expect("create task 1");
        store
            .create(TaskCreateInput {
                subject: "Build".to_string(),
                description: String::new(),
                owner: String::new(),
                blocked_by: vec![1],
            })
            .expect("create task 2");
        store
            .create(TaskCreateInput {
                subject: "Review".to_string(),
                description: String::new(),
                owner: String::new(),
                blocked_by: Vec::new(),
            })
            .expect("create task 3");
        store
            .update(
                parse_task_update_input(serde_json::json!({
                    "taskId": 3,
                    "status": "in_progress"
                }))
                .expect("parse update"),
            )
            .expect("update task 3");
        store
            .update(
                parse_task_update_input(serde_json::json!({
                    "taskId": 1,
                    "status": "completed"
                }))
                .expect("parse update"),
            )
            .expect("complete task 1");

        let listed = store.list().expect("list tasks");
        let listed = serde_json::from_str::<serde_json::Value>(&listed).expect("parse output");
        assert_eq!(listed["ready"].as_array().expect("ready").len(), 1);
        assert_eq!(listed["blocked"].as_array().expect("blocked").len(), 0);
        assert_eq!(
            listed["inProgress"].as_array().expect("in progress").len(),
            1
        );
        assert_eq!(listed["completed"].as_array().expect("completed").len(), 1);
    }

    #[test]
    fn completion_unblocks_and_reopen_reblocks_dependents() {
        let store = TaskStore::new(temp_dir("reblock"));

        store
            .create(TaskCreateInput {
                subject: "A".to_string(),
                description: String::new(),
                owner: String::new(),
                blocked_by: Vec::new(),
            })
            .expect("create task 1");
        store
            .create(TaskCreateInput {
                subject: "B".to_string(),
                description: String::new(),
                owner: String::new(),
                blocked_by: vec![1],
            })
            .expect("create task 2");

        let completed = store
            .update(
                parse_task_update_input(serde_json::json!({
                    "taskId": 1,
                    "status": "completed"
                }))
                .expect("parse update"),
            )
            .expect("complete task 1");
        let completed =
            serde_json::from_str::<serde_json::Value>(&completed).expect("parse completed");
        assert_eq!(
            completed["unblocked"].as_array().expect("unblocked").len(),
            1
        );

        let reopened = store
            .update(
                parse_task_update_input(serde_json::json!({
                    "taskId": 1,
                    "status": "pending"
                }))
                .expect("parse update"),
            )
            .expect("reopen task 1");
        let reopened =
            serde_json::from_str::<serde_json::Value>(&reopened).expect("parse reopened");
        assert_eq!(
            reopened["reblocked"].as_array().expect("reblocked").len(),
            1
        );
    }

    #[test]
    fn adding_cycle_is_rejected() {
        let store = TaskStore::new(temp_dir("cycle"));

        store
            .create(TaskCreateInput {
                subject: "A".to_string(),
                description: String::new(),
                owner: String::new(),
                blocked_by: Vec::new(),
            })
            .expect("create task 1");
        store
            .create(TaskCreateInput {
                subject: "B".to_string(),
                description: String::new(),
                owner: String::new(),
                blocked_by: vec![1],
            })
            .expect("create task 2");

        let error = store
            .update(
                parse_task_update_input(serde_json::json!({
                    "taskId": 1,
                    "addBlockedBy": [2]
                }))
                .expect("parse update"),
            )
            .expect_err("cycle should fail");
        assert!(error.to_string().contains("would create a cycle"));
    }

    #[test]
    fn blocked_task_cannot_start_or_complete() {
        let store = TaskStore::new(temp_dir("blocked-status"));

        store
            .create(TaskCreateInput {
                subject: "A".to_string(),
                description: String::new(),
                owner: String::new(),
                blocked_by: Vec::new(),
            })
            .expect("create task 1");
        store
            .create(TaskCreateInput {
                subject: "B".to_string(),
                description: String::new(),
                owner: String::new(),
                blocked_by: vec![1],
            })
            .expect("create task 2");

        let error = store
            .update(
                parse_task_update_input(serde_json::json!({
                    "taskId": 2,
                    "status": "in_progress"
                }))
                .expect("parse update"),
            )
            .expect_err("blocked task should fail");
        assert!(
            error
                .to_string()
                .contains("cannot be InProgress while blocked")
        );
    }

    #[test]
    fn parse_helpers_reject_bad_input() {
        assert!(parse_task_create_input(serde_json::json!({ "subject": "" })).is_err());
        assert!(
            parse_task_update_input(serde_json::json!({ "taskId": 1, "bogus": true })).is_err()
        );
        assert!(parse_task_list_input(serde_json::json!({ "bogus": true })).is_err());
    }

    #[test]
    fn completed_blocker_stays_out_of_unresolved_blocked_by() {
        let store = TaskStore::new(temp_dir("completed-blocker"));

        store
            .create(TaskCreateInput {
                subject: "A".to_string(),
                description: String::new(),
                owner: String::new(),
                blocked_by: Vec::new(),
            })
            .expect("create task 1");
        store
            .update(
                parse_task_update_input(serde_json::json!({
                    "taskId": 1,
                    "status": "completed"
                }))
                .expect("parse update"),
            )
            .expect("complete task 1");
        store
            .create(TaskCreateInput {
                subject: "B".to_string(),
                description: String::new(),
                owner: String::new(),
                blocked_by: vec![1],
            })
            .expect("create task 2");

        let tasks = store.load_all().expect("load tasks");
        assert_eq!(tasks[1].status, TaskStatus::Pending);
        assert!(tasks[1].blocked_by.is_empty());
        assert_eq!(tasks[0].blocks, vec![2]);
    }

    fn temp_dir(label: &str) -> PathBuf {
        let unique = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("mentra-task-graph-{label}-{timestamp}-{unique}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }
}
