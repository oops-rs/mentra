use std::sync::Arc;

use crate::agent::AgentEvent;

use super::BackgroundTaskSummary;

pub(crate) trait BackgroundObserverSink: Send + Sync {
    fn publish_snapshot(&self, tasks: &[BackgroundTaskSummary]);
    fn publish_event(&self, event: AgentEvent);
}

#[derive(Clone)]
pub(crate) struct BackgroundRegistration {
    pub(crate) agent_id: String,
    pub(crate) observer: Arc<dyn BackgroundObserverSink>,
}
