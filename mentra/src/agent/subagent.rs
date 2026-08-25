use std::{borrow::Cow, collections::HashSet, sync::Arc};

use crate::{
    ModelInfo, Role,
    error::RuntimeError,
    provider::Provider,
    runtime::{RuntimeIntrinsicTool, handle::RuntimeHandle},
};

use super::{
    Agent, AgentConfig, AgentSpawnOptions, SpawnedAgentStatus, SpawnedAgentSummary,
    TeammateIdentity, ToolProfile,
};

const SUBAGENT_MAX_ROUNDS: usize = 30;
const SUBAGENT_SYSTEM_PROMPT: &str = "You are a subagent working for another agent. Solve the delegated task, use tools when helpful, and finish with a concise final answer for the parent agent.";

/// A parent agent's blueprint for a disposable child, cloned from its own
/// config and overridable before spawning.
///
/// [`from_agent`](Self::from_agent) starts from an exact copy of the parent —
/// same model, tool profile, and system prompt — which is what makes the
/// default (no overrides) path byte-identical to the parent spawning a plain
/// subagent. `with_tool_profile`, `with_model`, and `with_system` each replace
/// one field of that clone so a delegating parent can hand a child a narrower
/// tool roster, a cheaper model, or a different system prompt without losing
/// the depth-guard and bounds-inheritance treatment [`spawn`](Self::spawn)
/// applies uniformly on top, regardless of which fields were overridden.
#[derive(Clone)]
pub struct DisposableSubagentTemplate {
    runtime: RuntimeHandle,
    /// The `Agent::id` of the agent [`from_agent`](Self::from_agent) was
    /// built from, checked by [`verify_source`](Self::verify_source) against
    /// whichever agent actually tries to spawn from this template.
    ///
    /// A template is a value with no lifetime tied to its source: nothing
    /// stops it from crossing to a different agent, session, or runtime than
    /// the one it was cloned from. Spawning it there would wire the "child"
    /// to the template's original runtime, name, and teammate identity while
    /// the receiver announces and registers it as its own — escaping the
    /// receiver's policy entirely once the runtimes differ. Every
    /// `spawn_subagent_from` entry point must verify the source before
    /// calling [`spawn`](Self::spawn).
    source_agent_id: String,
    model: String,
    context_window: Option<usize>,
    parent_name: String,
    config: AgentConfig,
    provider: Arc<dyn Provider>,
    hidden_tools: HashSet<String>,
    teammate_identity: Option<TeammateIdentity>,
    /// Pending replacement for `model`/`context_window`/`provider`, applied by
    /// [`spawn`](Self::spawn) once it can resolve `model.provider` against the
    /// runtime and report failure through the `Result` it already returns —
    /// the same failure mode [`Agent::set_model`](super::Agent::set_model)
    /// reports for the same lookup, kept here rather than in `with_model`
    /// itself so the builder chain stays infallible.
    model_override: Option<ModelInfo>,
}

impl DisposableSubagentTemplate {
    pub(crate) fn from_agent(agent: &Agent) -> Self {
        Self {
            runtime: agent.runtime.clone(),
            source_agent_id: agent.id.clone(),
            model: agent.model.clone(),
            context_window: agent.context_window,
            parent_name: agent.name.clone(),
            config: agent.config.clone(),
            provider: Arc::clone(&agent.provider),
            hidden_tools: agent.hidden_tools.clone(),
            teammate_identity: agent.teammate_identity.clone(),
            model_override: None,
        }
    }

    /// Confirms `receiver_agent_id`, on `receiver_runtime`, is the agent (and
    /// runtime) this template was built from — the check every
    /// `spawn_subagent_from` entry point runs before
    /// [`spawn`](Self::spawn), so a template handed to a different agent,
    /// session, or runtime is refused by name rather than silently spawning a
    /// child wired to its original source instead of its actual receiver.
    pub(crate) fn verify_source(
        &self,
        receiver_agent_id: &str,
        receiver_runtime: &RuntimeHandle,
    ) -> Result<(), RuntimeError> {
        if self.source_agent_id == receiver_agent_id
            && self.runtime.same_runtime_as(receiver_runtime)
        {
            return Ok(());
        }

        Err(RuntimeError::SubagentTemplateMismatch {
            template_source: self.source_agent_id.clone(),
            receiver: receiver_agent_id.to_string(),
        })
    }

    /// Replaces the tool profile the spawned child's config carries.
    ///
    /// This overrides `config.tool_profile` only — the spawn-level hidden-tools
    /// set (which always hides the `task` intrinsic from a subagent) is a
    /// separate mechanism applied by [`spawn`](Self::spawn) regardless of this
    /// override, exactly as for the un-overridden clone.
    pub fn with_tool_profile(mut self, tool_profile: ToolProfile) -> Self {
        self.config.tool_profile = tool_profile;
        self
    }

    /// Replaces the model (and, if it names a different provider, the
    /// provider) the spawned child runs on.
    ///
    /// Resolution against the runtime's registered providers happens in
    /// [`spawn`](Self::spawn), which can fail and already returns a `Result`.
    pub fn with_model(mut self, model: ModelInfo) -> Self {
        self.model_override = Some(model);
        self
    }

    /// Replaces the system prompt the spawned child's config carries, before
    /// [`spawn`](Self::spawn) appends the standard subagent instructions —
    /// the same suffix treatment the un-overridden clone's system prompt
    /// receives.
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.config.system = Some(system.into());
        self
    }

    pub(crate) fn spawn(&self) -> Result<Agent, RuntimeError> {
        let mut hidden_tools = self.hidden_tools.clone();
        hidden_tools.insert(RuntimeIntrinsicTool::Task.to_string());

        let mut config = self.config.clone();
        config.system = Some(build_subagent_system_prompt(
            self.config.system.as_deref().map(Cow::Borrowed),
        ));

        let (model, context_window, provider) = match &self.model_override {
            Some(model) => {
                let provider = self
                    .runtime
                    .get_provider(Some(&model.provider))
                    .ok_or_else(|| RuntimeError::ProviderNotFound(Some(model.provider.clone())))?;
                (model.id.clone(), model.context_window, provider)
            }
            None => (
                self.model.clone(),
                self.context_window,
                Arc::clone(&self.provider),
            ),
        };

        Agent::new(
            self.runtime.clone(),
            model,
            context_window,
            format!("{}::task", self.parent_name),
            config,
            provider,
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

    /// The template-taking sibling of [`spawn_subagent`](Self::spawn_subagent):
    /// spawns from a template the caller built (and possibly overrode) rather
    /// than an exact clone of this agent's own config, after confirming this
    /// agent actually is the template's source (see
    /// [`DisposableSubagentTemplate::verify_source`]).
    pub(crate) fn spawn_subagent_from(
        &self,
        template: DisposableSubagentTemplate,
    ) -> Result<Self, RuntimeError> {
        template.verify_source(&self.id, &self.runtime)?;
        template.spawn()
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
