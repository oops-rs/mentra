use std::collections::{BTreeSet, HashMap, HashSet};

use super::{
    TaskError,
    types::{TaskItem, TaskStatus},
};

pub(crate) fn has_unfinished_tasks(tasks: &[TaskItem]) -> bool {
    tasks
        .iter()
        .any(|task| task.status != TaskStatus::Completed)
}

pub(super) fn apply_status_change(
    tasks: &mut [TaskItem],
    task_id: u64,
    original_status: TaskStatus,
    next_status: TaskStatus,
    unblocked: &mut Vec<TaskItem>,
    reblocked: &mut Vec<TaskItem>,
) -> Result<(), TaskError> {
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

pub(super) fn add_dependency(
    tasks: &mut [TaskItem],
    blocker_id: u64,
    dependent_id: u64,
) -> Result<(), TaskError> {
    if blocker_id == dependent_id {
        return Err(TaskError::Validation(
            "Tasks cannot depend on themselves".to_string(),
        ));
    }

    let blocker_status = find_task(tasks, blocker_id)?.status.clone();
    let dependent_status = find_task(tasks, dependent_id)?.status.clone();

    let edge_exists = find_task(tasks, blocker_id)?.blocks.contains(&dependent_id);
    if !edge_exists && path_exists(tasks, dependent_id, blocker_id) {
        return Err(TaskError::Validation(format!(
            "Adding dependency {blocker_id} -> {dependent_id} would create a cycle"
        )));
    }

    insert_id(&mut find_task_mut(tasks, blocker_id)?.blocks, dependent_id);

    if blocker_status != TaskStatus::Completed {
        if dependent_status != TaskStatus::Pending {
            return Err(TaskError::Validation(format!(
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

pub(super) fn remove_dependency(
    tasks: &mut [TaskItem],
    blocker_id: u64,
    dependent_id: u64,
) -> Result<(), TaskError> {
    find_task(tasks, blocker_id)?;
    find_task(tasks, dependent_id)?;

    remove_id(&mut find_task_mut(tasks, blocker_id)?.blocks, dependent_id);
    remove_id(
        &mut find_task_mut(tasks, dependent_id)?.blocked_by,
        blocker_id,
    );
    Ok(())
}

pub(super) fn find_task(tasks: &[TaskItem], task_id: u64) -> Result<&TaskItem, TaskError> {
    tasks
        .iter()
        .find(|task| task.id == task_id)
        .ok_or_else(|| TaskError::Validation(format!("Task {task_id} does not exist")))
}

pub(super) fn sort_and_dedup_ids(ids: &mut Vec<u64>) {
    let unique = ids.iter().copied().collect::<BTreeSet<_>>();
    ids.clear();
    ids.extend(unique);
}

pub(super) fn sort_tasks(tasks: &mut [TaskItem]) {
    tasks.sort_by_key(|task| task.id);
}

fn validate_unblocked_status(task: &TaskItem) -> Result<(), TaskError> {
    if matches!(task.status, TaskStatus::InProgress | TaskStatus::Completed)
        && !task.blocked_by.is_empty()
    {
        return Err(TaskError::Validation(format!(
            "Task {} cannot be {} while blocked by {:?}",
            task.id, task.status, task.blocked_by
        )));
    }

    Ok(())
}

fn find_task_mut(tasks: &mut [TaskItem], task_id: u64) -> Result<&mut TaskItem, TaskError> {
    tasks
        .iter_mut()
        .find(|task| task.id == task_id)
        .ok_or_else(|| TaskError::Validation(format!("Task {task_id} does not exist")))
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
