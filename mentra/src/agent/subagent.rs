use std::{borrow::Cow, collections::HashSet, sync::Arc};

use crate::{
    Role,
    error::RuntimeError,
    provider::Provider,
    runtime::{RuntimeIntrinsicTool, handle::RuntimeHandle},
};

use super::{
    Agent, AgentConfig, AgentSpawnOptions, SpawnedAgentStatus, SpawnedAgentSummary,
    TeammateIdentity,
};

const SUBAGENT_MAX_ROUNDS: usize = 30;
const SUBAGENT_SYSTEM_PROMPT: &str = "You are a subagent working for another agent. Solve the delegated task, use tools when helpful, and finish with a concise final answer for the parent agent.";

#[derive(Clone)]
pub(crate) struct DisposableSubagentTemplate {
    runtime: RuntimeHandle,
    model: String,
    context_window: Option<usize>,
    parent_name: String,
    config: AgentConfig,
    provider: Arc<dyn Provider>,
    hidden_tools: HashSet<String>,
    teammate_identity: Option<TeammateIdentity>,
}

impl DisposableSubagentTemplate {
    pub(crate) fn from_agent(agent: &Agent) -> Self {
        Self {
            runtime: agent.runtime.clone(),
            model: agent.model.clone(),
            context_window: agent.context_window,
            parent_name: agent.name.clone(),
            config: agent.config.clone(),
            provider: Arc::clone(&agent.provider),
            hidden_tools: agent.hidden_tools.clone(),
            teammate_identity: agent.teammate_identity.clone(),
        }
    }

    pub(crate) fn spawn(&self) -> Result<Agent, RuntimeError> {
        let mut hidden_tools = self.hidden_tools.clone();
        hidden_tools.insert(RuntimeIntrinsicTool::Task.to_string());

        let mut config = self.config.clone();
        config.system = Some(build_subagent_system_prompt(
            self.config.system.as_deref().map(Cow::Borrowed),
        ));

        Agent::new(
            self.runtime.clone(),
            self.model.clone(),
            self.context_window,
            format!("{}::task", self.parent_name),
            config,
            Arc::clone(&self.provider),
            AgentSpawnOptions {
                hidden_tools,
                max_rounds: Some(SUBAGENT_MAX_ROUNDS),
                teammate_identity: self.teammate_identity.clone(),
            },
        )
    }
}

impl Agent {
    pub(crate) fn spawn_subagent(&self) -> Result<Self, RuntimeError> {
        self.disposable_subagent_template().spawn()
    }

    pub(crate) fn disposable_subagent_template(&self) -> DisposableSubagentTemplate {
        DisposableSubagentTemplate::from_agent(self)
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

pub(super) fn build_subagent_system_prompt(base: Option<Cow<'_, str>>) -> String {
    match base {
        Some(system) => format!("{system}\n\n{SUBAGENT_SYSTEM_PROMPT}"),
        None => SUBAGENT_SYSTEM_PROMPT.to_string(),
    }
}
