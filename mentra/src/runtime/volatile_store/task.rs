use std::{collections::HashMap, path::Path};

use crate::runtime::{RuntimeError, TaskItem, TaskStateSnapshot, TaskStore};

use super::{VolatileRuntimeStore, path_key};

/// Tasks namespaced by the caller-supplied `tasks_dir` path, mirroring the
/// default store's `tasks` table (keyed by the same string).
#[derive(Default)]
pub(super) struct TaskState {
    by_namespace: HashMap<String, Vec<TaskItem>>,
}

impl TaskStore for VolatileRuntimeStore {
    fn load_tasks(&self, namespace: &Path) -> Result<Vec<TaskItem>, RuntimeError> {
        Ok(self
            .lock()
            .tasks
            .by_namespace
            .get(&path_key(namespace))
            .cloned()
            .unwrap_or_default())
    }

    fn capture_tasks(&self, namespace: &Path) -> Result<TaskStateSnapshot, RuntimeError> {
        Ok(TaskStateSnapshot {
            tasks: self.load_tasks(namespace)?,
        })
    }

    fn restore_tasks(
        &self,
        namespace: &Path,
        snapshot: &TaskStateSnapshot,
    ) -> Result<(), RuntimeError> {
        self.replace_tasks(namespace, &snapshot.tasks)
    }

    fn replace_tasks(&self, namespace: &Path, tasks: &[TaskItem]) -> Result<(), RuntimeError> {
        self.lock()
            .tasks
            .by_namespace
            .insert(path_key(namespace), tasks.to_vec());
        Ok(())
    }

    fn mutate(
        &self,
        namespace: &Path,
        mutation: &mut dyn FnMut(&mut Vec<TaskItem>) -> Result<(), RuntimeError>,
    ) -> Result<(), RuntimeError> {
        let mut state = self.lock();
        let key = path_key(namespace);
        let mut tasks = state
            .tasks
            .by_namespace
            .get(&key)
            .cloned()
            .unwrap_or_default();
        mutation(&mut tasks)?;
        state.tasks.by_namespace.insert(key, tasks);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::runtime::{TaskItem, TaskStatus, TaskStore};

    use super::super::VolatileRuntimeStore;

    fn task(id: u64, subject: &str) -> TaskItem {
        TaskItem {
            id,
            subject: subject.to_string(),
            description: String::new(),
            status: TaskStatus::Pending,
            blocked_by: Vec::new(),
            blocks: Vec::new(),
            owner: String::new(),
            working_directory: None,
        }
    }

    #[test]
    fn load_tasks_reads_own_writes_and_stays_namespaced() {
        let store = VolatileRuntimeStore::new();
        let namespace = PathBuf::from("/tmp/does-not-exist/tasks");
        let item = task(1, "write the report");

        store
            .replace_tasks(&namespace, std::slice::from_ref(&item))
            .expect("replace tasks");

        assert_eq!(
            store.load_tasks(&namespace).expect("load tasks"),
            vec![item]
        );
        assert!(
            store
                .load_tasks(&PathBuf::from("/tmp/does-not-exist/other"))
                .expect("load unrelated namespace")
                .is_empty()
        );
    }

    #[test]
    fn capture_and_restore_round_trip() {
        let store = VolatileRuntimeStore::new();
        let namespace = PathBuf::from("/tmp/does-not-exist/tasks-2");
        let item = task(1, "first");

        store
            .replace_tasks(&namespace, std::slice::from_ref(&item))
            .expect("seed");
        let snapshot = store.capture_tasks(&namespace).expect("capture");

        store.replace_tasks(&namespace, &[]).expect("clear");
        assert!(store.load_tasks(&namespace).expect("load empty").is_empty());

        store.restore_tasks(&namespace, &snapshot).expect("restore");
        assert_eq!(
            store.load_tasks(&namespace).expect("load restored"),
            vec![item]
        );
    }

    #[test]
    fn failed_mutation_does_not_install_partial_changes() {
        let store = VolatileRuntimeStore::new();
        let namespace = PathBuf::from("/tmp/does-not-exist/tasks-rollback");
        let item = task(1, "original");
        store
            .replace_tasks(&namespace, std::slice::from_ref(&item))
            .expect("seed");

        let mut mutation = |tasks: &mut Vec<TaskItem>| {
            tasks[0].subject = "partial".to_string();
            Err(crate::runtime::RuntimeError::InvalidTask(
                "reject mutation".to_string(),
            ))
        };
        store
            .mutate(&namespace, &mut mutation)
            .expect_err("mutation should fail");

        assert_eq!(
            store.load_tasks(&namespace).expect("load tasks"),
            vec![item]
        );
    }
}
