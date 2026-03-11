use std::{borrow::Cow, sync::Arc};

use crate::{
    provider::model::{ContentBlock, Role},
    runtime::{
        TASK_TOOL_NAME,
        error::RuntimeError,
        task::{SUBAGENT_MAX_ROUNDS, build_subagent_system_prompt},
    },
};

use super::{Agent, SpawnedAgentStatus, SpawnedAgentSummary};

impl Agent {
    pub(crate) fn spawn_subagent(&self) -> Result<Self, RuntimeError> {
        let mut hidden_tools = self.hidden_tools.clone();
        hidden_tools.insert(TASK_TOOL_NAME.to_string());

        let mut config = self.config.clone();
        config.system = Some(build_subagent_system_prompt(
            self.config.system.as_deref().map(Cow::Borrowed),
        ));

        Self::new(
            self.runtime.clone(),
            self.model.clone(),
            format!("{}::task", self.name),
            config,
            Arc::clone(&self.provider),
            hidden_tools,
            Some(SUBAGENT_MAX_ROUNDS),
        )
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

        let text = message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                ContentBlock::Image { .. } => None,
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");

        if text.is_empty() {
            "(no summary)".to_string()
        } else {
            text
        }
    }
}
