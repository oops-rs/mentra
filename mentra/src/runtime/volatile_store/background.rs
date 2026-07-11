use std::collections::HashMap;

use crate::{
    background::{BackgroundNotification, BackgroundStore, BackgroundTaskSummary},
    runtime::RuntimeError,
};

use super::{DeliveryState, VolatileRuntimeStore};

struct BackgroundJobEntry {
    task: BackgroundTaskSummary,
    notification_state: DeliveryState,
}

/// Background-task notifications keyed by `(agent_id, task_id)`, mirroring
/// the default store's `background_jobs` table.
#[derive(Default)]
pub(super) struct BackgroundState {
    jobs: HashMap<(String, String), BackgroundJobEntry>,
}

fn notification_state_from_raw(value: i64) -> DeliveryState {
    match value {
        0 => DeliveryState::Pending,
        1 => DeliveryState::Inflight,
        _ => DeliveryState::Acked,
    }
}

impl BackgroundStore for VolatileRuntimeStore {
    fn load_background_tasks(
        &self,
        agent_id: &str,
    ) -> Result<Vec<BackgroundTaskSummary>, RuntimeError> {
        let state = self.lock();
        let mut tasks: Vec<_> = state
            .background
            .jobs
            .iter()
            .filter(|((aid, _), _)| aid == agent_id)
            .map(|(_, entry)| entry.task.clone())
            .collect();
        tasks.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(tasks)
    }

    fn upsert_background_task(
        &self,
        agent_id: &str,
        task: &BackgroundTaskSummary,
        notification_state: i64,
    ) -> Result<(), RuntimeError> {
        self.lock().background.jobs.insert(
            (agent_id.to_string(), task.id.clone()),
            BackgroundJobEntry {
                task: task.clone(),
                notification_state: notification_state_from_raw(notification_state),
            },
        );
        Ok(())
    }

    fn drain_background_notifications(
        &self,
        agent_id: &str,
    ) -> Result<Vec<BackgroundNotification>, RuntimeError> {
        let mut state = self.lock();
        let mut out = Vec::new();
        for ((aid, _id), entry) in state.background.jobs.iter_mut() {
            if aid == agent_id && entry.notification_state == DeliveryState::Pending {
                entry.notification_state = DeliveryState::Inflight;
                out.push(BackgroundNotification {
                    task_id: entry.task.id.clone(),
                    command: entry.task.command.clone(),
                    cwd: entry.task.cwd.clone(),
                    status: entry.task.status.clone(),
                    output_preview: entry
                        .task
                        .output_preview
                        .clone()
                        .unwrap_or_else(|| "(no output)".to_string()),
                });
            }
        }
        Ok(out)
    }

    fn has_deliverable_background_notifications(
        &self,
        agent_id: &str,
    ) -> Result<bool, RuntimeError> {
        Ok(self.lock().background.jobs.iter().any(|((aid, _), entry)| {
            aid == agent_id && entry.notification_state == DeliveryState::Pending
        }))
    }

    fn has_pending_background_notifications(&self, agent_id: &str) -> Result<bool, RuntimeError> {
        Ok(self.lock().background.jobs.iter().any(|((aid, _), entry)| {
            aid == agent_id
                && matches!(
                    entry.notification_state,
                    DeliveryState::Pending | DeliveryState::Inflight
                )
        }))
    }

    fn ack_background_notifications(&self, agent_id: &str) -> Result<(), RuntimeError> {
        let mut state = self.lock();
        for ((aid, _), entry) in state.background.jobs.iter_mut() {
            if aid == agent_id && entry.notification_state == DeliveryState::Inflight {
                entry.notification_state = DeliveryState::Acked;
            }
        }
        Ok(())
    }

    fn requeue_background_notifications(&self, agent_id: &str) -> Result<(), RuntimeError> {
        let mut state = self.lock();
        for ((aid, _), entry) in state.background.jobs.iter_mut() {
            if aid == agent_id && entry.notification_state == DeliveryState::Inflight {
                entry.notification_state = DeliveryState::Pending;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::background::{BackgroundStore, BackgroundTaskStatus, BackgroundTaskSummary};

    use super::super::VolatileRuntimeStore;

    fn summary(id: &str) -> BackgroundTaskSummary {
        BackgroundTaskSummary {
            id: id.to_string(),
            command: "echo hi".to_string(),
            cwd: PathBuf::from("/tmp"),
            status: BackgroundTaskStatus::Running,
            output_preview: None,
        }
    }

    #[test]
    fn drain_then_ack_notifications() {
        let store = VolatileRuntimeStore::new();
        store
            .upsert_background_task("agent-1", &summary("bg-1"), 0)
            .expect("seed pending task");

        assert!(
            store
                .has_deliverable_background_notifications("agent-1")
                .expect("has deliverable")
        );

        let drained = store
            .drain_background_notifications("agent-1")
            .expect("drain notifications");
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].task_id, "bg-1");

        // Draining moves the notification to in-flight; it is no longer
        // freshly deliverable, but it is still pending overall.
        assert!(
            !store
                .has_deliverable_background_notifications("agent-1")
                .expect("has deliverable after drain")
        );
        assert!(
            store
                .has_pending_background_notifications("agent-1")
                .expect("has pending after drain")
        );

        store
            .ack_background_notifications("agent-1")
            .expect("ack notifications");
        assert!(
            !store
                .has_pending_background_notifications("agent-1")
                .expect("has pending after ack")
        );
    }

    #[test]
    fn background_tasks_are_scoped_per_agent() {
        let store = VolatileRuntimeStore::new();
        store
            .upsert_background_task("agent-a", &summary("bg-1"), 2)
            .expect("seed agent a");
        store
            .upsert_background_task("agent-b", &summary("bg-1"), 2)
            .expect("seed agent b");

        assert_eq!(
            store
                .load_background_tasks("agent-a")
                .expect("load agent a")
                .len(),
            1
        );
        assert_eq!(
            store
                .load_background_tasks("agent-b")
                .expect("load agent b")
                .len(),
            1
        );
    }
}
