use std::borrow::Cow;

use crate::runtime::{
    TaskStateSnapshot,
    error::RuntimeError,
    task::{TASK_REMINDER_TEXT, TaskAccess, TaskIntrinsicTool, has_unfinished_tasks},
};

use super::Agent;

impl Agent {
    pub(crate) fn effective_system_prompt(&self) -> Option<Cow<'_, str>> {
        let mut sections = Vec::new();

        if self.rounds_since_task >= self.config.task.reminder_threshold
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

    pub(crate) fn note_round_without_task(&mut self) {
        if has_unfinished_tasks(&self.tasks) {
            self.rounds_since_task += 1;
        }
    }

    pub(crate) fn record_task_activity(&mut self) {
        self.rounds_since_task = 0;
    }

    pub(crate) fn refresh_tasks_from_disk(&mut self) -> Result<(), RuntimeError> {
        let tasks = self
            .runtime
            .store()
            .load_tasks(self.config.task.tasks_dir.as_path())?;
        self.tasks = tasks;
        let tasks = self.tasks.clone();
        self.mutate_snapshot(|snapshot| {
            snapshot.tasks = tasks;
        });
        Ok(())
    }

    pub(crate) fn task_access(&self) -> TaskAccess<'_> {
        match &self.teammate_identity {
            Some(_) => TaskAccess::Teammate(self.name.as_str()),
            None => TaskAccess::Lead,
        }
    }

    pub(crate) fn try_claim_ready_task(
        &mut self,
    ) -> Result<Option<crate::runtime::TaskItem>, RuntimeError> {
        self.refresh_tasks_from_disk()?;
        if self.owns_unfinished_tasks() {
            return Ok(None);
        }

        match self.execute_task_mutation(&TaskIntrinsicTool::Claim, serde_json::json!({})) {
            Ok(content) => {
                self.refresh_tasks_from_disk()?;
                serde_json::from_str::<crate::runtime::TaskItem>(&content)
                    .map(Some)
                    .map_err(RuntimeError::FailedToSerializeTasks)
            }
            Err(error) if error == "No ready unowned tasks are available to claim" => Ok(None),
            Err(error) => Err(RuntimeError::InvalidTask(error)),
        }
    }

    pub(crate) fn execute_task_mutation(
        &self,
        tool: &TaskIntrinsicTool,
        input: serde_json::Value,
    ) -> Result<String, String> {
        self.runtime.execute_task_mutation(
            tool,
            input,
            self.config.task.tasks_dir.as_path(),
            self.task_access(),
        )
    }

    pub(super) fn capture_task_disk_state(&self) -> Result<TaskStateSnapshot, RuntimeError> {
        self.runtime
            .store()
            .capture_tasks(self.config.task.tasks_dir.as_path())
    }

    fn owns_unfinished_tasks(&self) -> bool {
        self.tasks.iter().any(|task| {
            task.owner == self.name && !matches!(task.status, crate::runtime::TaskStatus::Completed)
        })
    }

    pub(super) fn restore_task_state(
        &mut self,
        tasks: Vec<crate::runtime::TaskItem>,
        rounds_since_task: usize,
        disk_state: &TaskStateSnapshot,
    ) -> Result<(), RuntimeError> {
        self.runtime
            .store()
            .restore_tasks(self.config.task.tasks_dir.as_path(), disk_state)?;
        self.tasks = tasks;
        self.rounds_since_task = rounds_since_task;
        let tasks = self.tasks.clone();
        self.mutate_snapshot(|snapshot| {
            snapshot.tasks = tasks;
        });
        Ok(())
    }
}
