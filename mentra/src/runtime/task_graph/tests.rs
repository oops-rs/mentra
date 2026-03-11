use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use super::{
    input::{
        TaskCreateInput, parse_task_create_input, parse_task_list_input, parse_task_update_input,
    },
    store::TaskStore,
    types::TaskStatus,
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
    let completed = serde_json::from_str::<serde_json::Value>(&completed).expect("parse completed");
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
    let reopened = serde_json::from_str::<serde_json::Value>(&reopened).expect("parse reopened");
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
    assert!(parse_task_update_input(serde_json::json!({ "taskId": 1, "bogus": true })).is_err());
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
    let path = std::env::temp_dir().join(format!("mentra-task-graph-{label}-{timestamp}-{unique}"));
    fs::create_dir_all(&path).expect("create temp dir");
    path
}
