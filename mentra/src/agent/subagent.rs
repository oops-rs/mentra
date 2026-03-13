use crate::{Role, error::RuntimeError};

use super::{Agent, SpawnedAgentStatus, SpawnedAgentSummary};

impl Agent {
    pub(crate) fn spawn_subagent(&self) -> Result<Self, RuntimeError> {
        self.spawn_disposable_subagent()
    }

    pub(crate) fn register_subagent(&mut self, agent: &Agent) -> SpawnedAgentSummary {
        let summary = SpawnedAgentSummary {
            id: agent.id.clone(),
            name: agent.name.clone(),
            model: agent.model.clone(),
            status: SpawnedAgentStatus::Running,
        };
        let summary_for_snapshot = summary.clone();
        self.mutate_snapshot(|snapshot| {
            snapshot.subagents.push(summary_for_snapshot);
        });
        summary
    }

    pub(crate) fn finish_subagent(
        &mut self,
        id: &str,
        status: SpawnedAgentStatus,
    ) -> Option<SpawnedAgentSummary> {
        let mut finished = None;
        self.mutate_snapshot(|snapshot| {
            if let Some(summary) = snapshot.subagents.iter_mut().find(|agent| agent.id == id) {
                summary.status = status;
                finished = Some(summary.clone());
            }
        });
        finished
    }

    pub(crate) fn final_text_summary(&self) -> String {
        let Some(message) = self.last_message() else {
            return "(no summary)".to_string();
        };

        if message.role != Role::Assistant {
            return "(no summary)".to_string();
        }

        let text = message.text();

        if text.is_empty() {
            "(no summary)".to_string()
        } else {
            text
        }
    }
}
