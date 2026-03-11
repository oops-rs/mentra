use std::borrow::Cow;

use crate::runtime::{
    TaskDiskState,
    error::RuntimeError,
    task_graph::{TASK_REMINDER_TEXT, TaskStore, has_unfinished_tasks},
};

use super::Agent;

impl Agent {
    pub(crate) fn effective_system_prompt(&self) -> Option<Cow<'_, str>> {
        let mut sections = Vec::new();

        if self.rounds_since_task_graph >= self.config.task_graph.reminder_threshold
            && has_unfinished_tasks(&self.tasks)
        {
            sections.push(TASK_REMINDER_TEXT.to_string());
        }

        if let Some(system) = &self.config.system {
            sections.push(system.clone());
        }

        if let Some(skills) = self.runtime.skill_descriptions() {
            sections.push(skills);
        }

        if sections.is_empty() {
            None
        } else {
            Some(Cow::Owned(sections.join("\n\n")))
        }
    }

    pub(crate) fn note_round_without_task_graph(&mut self) {
        if has_unfinished_tasks(&self.tasks) {
            self.rounds_since_task_graph += 1;
        }
    }

    pub(crate) fn record_task_graph_activity(&mut self) {
        self.rounds_since_task_graph = 0;
    }

    pub(crate) fn refresh_tasks_from_disk(&mut self) -> Result<(), RuntimeError> {
        let tasks = TaskStore::new(self.config.task_graph.tasks_dir.clone())
            .load_all()
            .map_err(map_task_graph_error_for_load)?;
        self.tasks = tasks;
        let tasks = self.tasks.clone();
        self.mutate_snapshot(|snapshot| {
            snapshot.tasks = tasks;
        });
        Ok(())
    }

    pub(super) fn capture_task_disk_state(&self) -> Result<TaskDiskState, RuntimeError> {
        TaskStore::new(self.config.task_graph.tasks_dir.clone())
            .capture_disk_state()
            .map_err(map_task_graph_error_for_load)
    }

    pub(super) fn restore_task_state(
        &mut self,
        tasks: Vec<crate::runtime::TaskItem>,
        rounds_since_task_graph: usize,
        disk_state: &TaskDiskState,
    ) -> Result<(), RuntimeError> {
        TaskStore::new(self.config.task_graph.tasks_dir.clone())
            .restore_disk_state(disk_state)
            .map_err(map_task_graph_error_for_restore)?;
        self.tasks = tasks;
        self.rounds_since_task_graph = rounds_since_task_graph;
        let tasks = self.tasks.clone();
        self.mutate_snapshot(|snapshot| {
            snapshot.tasks = tasks;
        });
        Ok(())
    }
}

fn map_task_graph_error_for_load(error: crate::runtime::TaskGraphError) -> RuntimeError {
    match error {
        crate::runtime::TaskGraphError::Io(error) => RuntimeError::FailedToLoadTasks(error),
        crate::runtime::TaskGraphError::Serde(error) => RuntimeError::FailedToSerializeTasks(error),
        crate::runtime::TaskGraphError::Validation(message) => {
            RuntimeError::InvalidTaskGraph(message)
        }
    }
}

fn map_task_graph_error_for_restore(error: crate::runtime::TaskGraphError) -> RuntimeError {
    match error {
        crate::runtime::TaskGraphError::Io(error) => RuntimeError::FailedToRestoreTasks(error),
        crate::runtime::TaskGraphError::Serde(error) => RuntimeError::FailedToSerializeTasks(error),
        crate::runtime::TaskGraphError::Validation(message) => {
            RuntimeError::InvalidTaskGraph(message)
        }
    }
}
