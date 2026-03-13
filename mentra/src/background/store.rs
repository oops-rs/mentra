use crate::error::RuntimeError;

use super::{BackgroundNotification, BackgroundTaskSummary};

pub trait BackgroundStore: Send + Sync {
    fn load_background_tasks(
        &self,
        agent_id: &str,
    ) -> Result<Vec<BackgroundTaskSummary>, RuntimeError>;
    fn upsert_background_task(
        &self,
        agent_id: &str,
        task: &BackgroundTaskSummary,
        notification_state: i64,
    ) -> Result<(), RuntimeError>;
    fn drain_background_notifications(
        &self,
        agent_id: &str,
    ) -> Result<Vec<BackgroundNotification>, RuntimeError>;
    fn ack_background_notifications(&self, agent_id: &str) -> Result<(), RuntimeError>;
    fn requeue_background_notifications(&self, agent_id: &str) -> Result<(), RuntimeError>;
}
