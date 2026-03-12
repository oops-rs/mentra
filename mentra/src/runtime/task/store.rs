use super::{
    TaskAccess, TaskError,
    input::TaskUpdateInput,
    types::{TaskItem, TaskStatus},
};

pub(super) fn validate_unblocked_status(task: &TaskItem) -> Result<(), TaskError> {
    if matches!(task.status, TaskStatus::InProgress | TaskStatus::Completed)
        && !task.blocked_by.is_empty()
    {
        return Err(TaskError::Validation(format!(
            "Task {} cannot be {:?} while blocked by {:?}",
            task.id, task.status, task.blocked_by
        )));
    }

    Ok(())
}

pub(super) fn validate_claimable(task: &TaskItem, owner: &str) -> Result<(), TaskError> {
    if !task.owner.is_empty() {
        return Err(TaskError::Validation(format!(
            "Task {} is already owned by '{}'",
            task.id, task.owner
        )));
    }
    if !task.blocked_by.is_empty() {
        return Err(TaskError::Validation(format!(
            "Task {} is blocked by {:?} and cannot be claimed",
            task.id, task.blocked_by
        )));
    }
    if task.status != TaskStatus::Pending {
        return Err(TaskError::Validation(format!(
            "Task {} is {:?} and cannot be claimed by '{}'",
            task.id, task.status, owner
        )));
    }

    Ok(())
}

pub(super) fn validate_update_access(
    task: &TaskItem,
    input: &TaskUpdateInput,
    access: TaskAccess<'_>,
) -> Result<(), TaskError> {
    match access {
        TaskAccess::Lead => Ok(()),
        TaskAccess::Teammate(name) if task.owner == name => {
            if updates_dependencies(input) {
                return Err(TaskError::Validation(format!(
                    "Teammate '{name}' cannot edit dependencies for task {}",
                    task.id
                )));
            }
            if let Some(owner) = &input.owner
                && owner != name
            {
                return Err(TaskError::Validation(format!(
                    "Teammate '{name}' cannot reassign task {} to '{}'",
                    task.id, owner
                )));
            }
            Ok(())
        }
        TaskAccess::Teammate(name) => Err(TaskError::Validation(format!(
            "Teammate '{name}' cannot update task {} owned by '{}'",
            task.id, task.owner
        ))),
    }
}

pub(super) fn is_claimable(task: &TaskItem) -> bool {
    task.status == TaskStatus::Pending && task.blocked_by.is_empty() && task.owner.is_empty()
}

pub(super) fn find_task_mut(tasks: &mut [TaskItem], task_id: u64) -> Result<&mut TaskItem, TaskError> {
    tasks
        .iter_mut()
        .find(|task| task.id == task_id)
        .ok_or_else(|| TaskError::Validation(format!("Task {task_id} does not exist")))
}

fn updates_dependencies(input: &TaskUpdateInput) -> bool {
    !input.add_blocked_by.is_empty()
        || !input.remove_blocked_by.is_empty()
        || !input.add_blocks.is_empty()
        || !input.remove_blocks.is_empty()
}
