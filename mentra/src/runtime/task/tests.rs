use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::json;

use crate::runtime::{SqliteRuntimeStore, TaskIntrinsicTool, TaskStore};

use super::{
    TaskAccess, TaskItem,
    input::{
        TaskCreateInput, TaskUpdateInput, parse_task_create_input, parse_task_list_input,
        parse_task_update_input,
    },
    types::TaskStatus,
};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

#[test]
fn create_and_list_group_ready_blocked_and_completed_tasks() {
    let store = TaskHarness::new("grouping");

    store.create(TaskCreateInput {
        subject: "Plan".to_string(),
        description: String::new(),
        owner: String::new(),
        working_directory: None,
        blocked_by: Vec::new(),
    });
    store.create(TaskCreateInput {
        subject: "Build".to_string(),
        description: String::new(),
        owner: String::new(),
        working_directory: None,
        blocked_by: vec![1],
    });
    store.create(TaskCreateInput {
        subject: "Review".to_string(),
        description: String::new(),
        owner: String::new(),
        working_directory: None,
        blocked_by: Vec::new(),
    });
    store.update(
        parse_task_update_input(json!({
            "taskId": 3,
            "status": "in_progress"
        }))
        .expect("parse update"),
        TaskAccess::Lead,
    );
    store.update(
        parse_task_update_input(json!({
            "taskId": 1,
            "status": "completed"
        }))
        .expect("parse update"),
        TaskAccess::Lead,
    );

    let listed = serde_json::from_str::<serde_json::Value>(&store.list()).expect("parse output");
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
    let store = TaskHarness::new("reblock");

    store.create(TaskCreateInput {
        subject: "A".to_string(),
        description: String::new(),
        owner: String::new(),
        working_directory: None,
        blocked_by: Vec::new(),
    });
    store.create(TaskCreateInput {
        subject: "B".to_string(),
        description: String::new(),
        owner: String::new(),
        working_directory: None,
        blocked_by: vec![1],
    });

    let completed = serde_json::from_str::<serde_json::Value>(
        &store.update(
            parse_task_update_input(json!({
                "taskId": 1,
                "status": "completed"
            }))
            .expect("parse update"),
            TaskAccess::Lead,
        ),
    )
    .expect("parse completed");
    assert_eq!(
        completed["unblocked"].as_array().expect("unblocked").len(),
        1
    );

    let reopened = serde_json::from_str::<serde_json::Value>(
        &store.update(
            parse_task_update_input(json!({
                "taskId": 1,
                "status": "pending"
            }))
            .expect("parse update"),
            TaskAccess::Lead,
        ),
    )
    .expect("parse reopened");
    assert_eq!(
        reopened["reblocked"].as_array().expect("reblocked").len(),
        1
    );
}

#[test]
fn adding_cycle_is_rejected() {
    let store = TaskHarness::new("cycle");

    store.create(TaskCreateInput {
        subject: "A".to_string(),
        description: String::new(),
        owner: String::new(),
        working_directory: None,
        blocked_by: Vec::new(),
    });
    store.create(TaskCreateInput {
        subject: "B".to_string(),
        description: String::new(),
        owner: String::new(),
        working_directory: None,
        blocked_by: vec![1],
    });

    let error = store
        .try_update(
            parse_task_update_input(json!({
                "taskId": 1,
                "addBlockedBy": [2]
            }))
            .expect("parse update"),
            TaskAccess::Lead,
        )
        .expect_err("cycle should fail");
    assert!(error.contains("would create a cycle"));
}

#[test]
fn blocked_task_cannot_start_or_complete() {
    let store = TaskHarness::new("blocked-status");

    store.create(TaskCreateInput {
        subject: "A".to_string(),
        description: String::new(),
        owner: String::new(),
        working_directory: None,
        blocked_by: Vec::new(),
    });
    store.create(TaskCreateInput {
        subject: "B".to_string(),
        description: String::new(),
        owner: String::new(),
        working_directory: None,
        blocked_by: vec![1],
    });

    let error = store
        .try_update(
            parse_task_update_input(json!({
                "taskId": 2,
                "status": "in_progress"
            }))
            .expect("parse update"),
            TaskAccess::Lead,
        )
        .expect_err("blocked task should fail");
    assert!(error.contains("cannot be in_progress while blocked"));
}

#[test]
fn parse_helpers_reject_bad_input() {
    assert!(parse_task_create_input(json!({ "subject": "" })).is_err());
    assert!(parse_task_update_input(json!({ "taskId": 1, "bogus": true })).is_err());
    assert!(parse_task_list_input(json!({ "bogus": true })).is_err());
}

#[test]
fn completed_blocker_stays_out_of_unresolved_blocked_by() {
    let store = TaskHarness::new("completed-blocker");

    store.create(TaskCreateInput {
        subject: "A".to_string(),
        description: String::new(),
        owner: String::new(),
        working_directory: None,
        blocked_by: Vec::new(),
    });
    store.update(
        parse_task_update_input(json!({
            "taskId": 1,
            "status": "completed"
        }))
        .expect("parse update"),
        TaskAccess::Lead,
    );
    store.create(TaskCreateInput {
        subject: "B".to_string(),
        description: String::new(),
        owner: String::new(),
        working_directory: None,
        blocked_by: vec![1],
    });

    let tasks = store.load_all();
    assert_eq!(tasks[1].status, TaskStatus::Pending);
    assert!(tasks[1].blocked_by.is_empty());
    assert_eq!(tasks[0].blocks, vec![2]);
}

#[test]
fn claim_first_ready_unowned_task() {
    let store = TaskHarness::new("claim-first");
    store.create(TaskCreateInput {
        subject: "A".to_string(),
        description: String::new(),
        owner: String::new(),
        working_directory: None,
        blocked_by: Vec::new(),
    });
    store.create(TaskCreateInput {
        subject: "B".to_string(),
        description: String::new(),
        owner: String::new(),
        working_directory: None,
        blocked_by: vec![1],
    });

    let claimed = serde_json::from_str::<serde_json::Value>(&store.claim(None, "alice"))
        .expect("parse claimed");
    assert_eq!(claimed["id"].as_u64(), Some(1));
    assert_eq!(claimed["owner"].as_str(), Some("alice"));
}

#[test]
fn claim_explicit_task_id() {
    let store = TaskHarness::new("claim-explicit");
    store.create(TaskCreateInput {
        subject: "A".to_string(),
        description: String::new(),
        owner: String::new(),
        working_directory: None,
        blocked_by: Vec::new(),
    });
    store.create(TaskCreateInput {
        subject: "B".to_string(),
        description: String::new(),
        owner: String::new(),
        working_directory: None,
        blocked_by: Vec::new(),
    });

    let claimed = serde_json::from_str::<serde_json::Value>(&store.claim(Some(2), "bob"))
        .expect("parse claimed");
    assert_eq!(claimed["id"].as_u64(), Some(2));
    assert_eq!(claimed["owner"].as_str(), Some("bob"));
}

#[test]
fn claim_rejects_unclaimable_tasks() {
    let store = TaskHarness::new("claim-reject");
    store.create(TaskCreateInput {
        subject: "A".to_string(),
        description: String::new(),
        owner: String::new(),
        working_directory: None,
        blocked_by: Vec::new(),
    });
    store.create(TaskCreateInput {
        subject: "B".to_string(),
        description: String::new(),
        owner: String::new(),
        working_directory: None,
        blocked_by: vec![1],
    });

    let blocked = store.try_claim(Some(2), "alice").expect_err("blocked task");
    assert!(blocked.contains("cannot be claimed"));

    store.claim(Some(1), "alice");
    let owned = store.try_claim(Some(1), "bob").expect_err("owned task");
    assert!(owned.contains("already owned"));

    let missing = store
        .try_claim(Some(99), "alice")
        .expect_err("missing task");
    assert!(missing.contains("does not exist"));

    let store = TaskHarness::new("claim-status");
    store.create(TaskCreateInput {
        subject: "C".to_string(),
        description: String::new(),
        owner: String::new(),
        working_directory: None,
        blocked_by: Vec::new(),
    });
    store.update(
        parse_task_update_input(json!({
            "taskId": 1,
            "status": "in_progress"
        }))
        .expect("parse update"),
        TaskAccess::Lead,
    );
    let in_progress = store
        .try_claim(Some(1), "alice")
        .expect_err("in progress task");
    assert!(in_progress.contains("cannot be claimed"));

    let store = TaskHarness::new("claim-completed");
    store.create(TaskCreateInput {
        subject: "D".to_string(),
        description: String::new(),
        owner: String::new(),
        working_directory: None,
        blocked_by: Vec::new(),
    });
    store.update(
        parse_task_update_input(json!({
            "taskId": 1,
            "status": "completed"
        }))
        .expect("parse update"),
        TaskAccess::Lead,
    );
    let completed = store
        .try_claim(Some(1), "alice")
        .expect_err("completed task");
    assert!(completed.contains("cannot be claimed"));
}

#[test]
fn teammate_cannot_edit_task_dependencies() {
    let store = TaskHarness::new("teammate-deps");
    store.create(TaskCreateInput {
        subject: "Owned".to_string(),
        description: String::new(),
        owner: "alice".to_string(),
        working_directory: None,
        blocked_by: Vec::new(),
    });
    store.create(TaskCreateInput {
        subject: "Other".to_string(),
        description: String::new(),
        owner: String::new(),
        working_directory: None,
        blocked_by: Vec::new(),
    });

    let error = store
        .try_update(
            parse_task_update_input(json!({
                "taskId": 1,
                "addBlocks": [2]
            }))
            .expect("parse update"),
            TaskAccess::Teammate("alice"),
        )
        .expect_err("dependency edit should fail");
    assert!(error.contains("cannot edit dependencies"));
}

struct TaskHarness {
    store: SqliteRuntimeStore,
    namespace: PathBuf,
}

impl TaskHarness {
    fn new(label: &str) -> Self {
        Self {
            store: temp_store(label),
            namespace: temp_namespace(label),
        }
    }

    fn create(&self, input: TaskCreateInput) -> String {
        self.try_create(input).expect("create task")
    }

    fn try_create(&self, input: TaskCreateInput) -> Result<String, String> {
        super::execute_with_store(
            &self.store,
            &super::TaskIntrinsicTool::Create,
            serde_json::to_value(input).expect("serialize task create input"),
            self.namespace.as_path(),
            TaskAccess::Lead,
        )
    }

    fn update(&self, input: TaskUpdateInput, access: TaskAccess<'_>) -> String {
        self.try_update(input, access).expect("update task")
    }

    fn try_update(&self, input: TaskUpdateInput, access: TaskAccess<'_>) -> Result<String, String> {
        super::execute_with_store(
            &self.store,
            &super::TaskIntrinsicTool::Update,
            serde_json::to_value(input).expect("serialize task update input"),
            self.namespace.as_path(),
            access,
        )
    }

    fn claim(&self, task_id: Option<u64>, owner: &str) -> String {
        self.try_claim(task_id, owner).expect("claim task")
    }

    fn try_claim(&self, task_id: Option<u64>, owner: &str) -> Result<String, String> {
        super::execute_with_store(
            &self.store,
            &TaskIntrinsicTool::Claim,
            json!({ "taskId": task_id }),
            self.namespace.as_path(),
            TaskAccess::Teammate(owner),
        )
    }

    fn list(&self) -> String {
        super::execute_with_store(
            &self.store,
            &TaskIntrinsicTool::List,
            json!({}),
            self.namespace.as_path(),
            TaskAccess::Lead,
        )
        .expect("list tasks")
    }

    fn load_all(&self) -> Vec<TaskItem> {
        self.store
            .load_tasks(self.namespace.as_path())
            .expect("load tasks")
    }
}

fn temp_namespace(label: &str) -> PathBuf {
    let path = temp_path(format!("mentra-task-graph-{label}"));
    fs::create_dir_all(&path).expect("create temp namespace dir");
    path
}

fn temp_store(label: &str) -> SqliteRuntimeStore {
    SqliteRuntimeStore::new(
        temp_path(format!("mentra-task-store-{label}")).with_extension("sqlite"),
    )
}

fn temp_path(label: String) -> PathBuf {
    let unique = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    std::env::temp_dir().join(format!("{label}-{timestamp}-{unique}"))
}
