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
/// one field of that clone so a delegating parent can hand a child a
/// different tool roster, a cheaper model, or a different system prompt
/// without losing the depth-guard and bounds-inheritance treatment
/// [`spawn`](Self::spawn) applies uniformly on top, regardless of which
/// fields were overridden.
///
/// Each `with_*` is a plain override, not an enforced narrowing: nothing here
/// checks a new tool profile against the parent's, so a child spawned with
/// fewer tools than its parent can still spawn its own grandchild with more
/// than it has itself, simply by calling `with_tool_profile` with a wider
/// profile than the one it was given. Confining what a whole delegation chain
/// may grant — a policy, not a mechanism — is left to the caller (see
/// `with_child_policy` in a host like basis) rather than built in here, since
/// enforcing it here would need profile-intersection machinery this type does
/// not have.
#[derive(Clone)]
#[must_use = "a template does nothing on its own -- spawn it with spawn_subagent_from, \
              or the override methods called on it are silently discarded"]
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
    /// override, exactly as for the un-overridden clone. It replaces the
    /// parent's profile outright rather than narrowing it: passing a wider
    /// `tool_profile` than the parent's own hands the child more tools than
    /// the parent has (see the type-level docs).
    #[must_use = "with_tool_profile returns a new template rather than mutating in place; \
                  a discarded return value leaves the override applied to nothing"]
    pub fn with_tool_profile(mut self, tool_profile: ToolProfile) -> Self {
        self.config.tool_profile = tool_profile;
        self
    }

    /// Replaces the model (and, if it names a different provider, the
    /// provider) the spawned child runs on.
    ///
    /// Resolution against the runtime's registered providers happens when the
    /// template is actually spawned, which can fail and already returns a
    /// `Result`. When `model.context_window` is left `None` — the default
    /// from [`ModelInfo::new`], as opposed to a `ModelInfo` read back from
    /// [`Runtime::list_models`](crate::Runtime::list_models) — spawning
    /// through `spawn_subagent_from` looks it up in the model's provider's own
    /// listing rather than leaving it unset; `spawn_subagent` never reaches
    /// this path, since it has no way to set an override at all.
    ///
    /// Everything else in `config` — including
    /// `config.provider_request_options` (reasoning effort, and other
    /// provider-specific request options) — travels with the parent's clone
    /// unchanged, even when `model` names a different provider family than
    /// the parent's own. [`Agent::set_model`](super::Agent::set_model) has the
    /// same behavior for the same reason: those options are a property of
    /// the agent's configured intent, not of any one provider, and it is the
    /// caller's job to know whether the destination provider understands
    /// them.
    #[must_use = "with_model returns a new template rather than mutating in place; \
                  a discarded return value leaves the override applied to nothing"]
    pub fn with_model(mut self, model: ModelInfo) -> Self {
        self.model_override = Some(model);
        self
    }

    /// Replaces the system prompt the spawned child's config carries, before
    /// [`spawn`](Self::spawn) appends the standard subagent instructions —
    /// the same suffix treatment the un-overridden clone's system prompt
    /// receives.
    #[must_use = "with_system returns a new template rather than mutating in place; \
                  a discarded return value leaves the override applied to nothing"]
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.config.system = Some(system.into());
        self
    }

    pub(crate) fn spawn(&self) -> Result<Agent, RuntimeError> {
        self.build_agent(self.model_override.clone())
    }

    /// The `spawn_subagent_from` family's spawn step: like [`spawn`](Self::spawn),
    /// but first fills in an overridden model's context window from the
    /// runtime's model listing when the caller left it unset.
    ///
    /// `ModelInfo::new(id, provider)` defaults `context_window` to `None`, so
    /// a hand-built override passed to
    /// [`with_model`](Self::with_model) — as opposed to one read back from
    /// [`Runtime::list_models`](crate::Runtime::list_models) — would otherwise
    /// silently degrade window-relative compaction for the child even when
    /// the runtime's own listing knows the real number. `spawn` (used only by
    /// `spawn_subagent`, which has no way to set an override) stays
    /// synchronous and does not do this lookup.
    pub(crate) async fn spawn_from(&self) -> Result<Agent, RuntimeError> {
        let model_override = match self.model_override.clone() {
            Some(model) if model.context_window.is_none() => {
                Some(self.listed_context_window(model).await)
            }
            other => other,
        };
        self.build_agent(model_override)
    }

    /// Looks `model` up in its provider's model listing and copies over the
    /// context window the listing reports, leaving `model` unchanged if the
    /// provider is unregistered, the listing call fails, or the listing does
    /// not mention this model id -- `spawn_from` still spawns the child in
    /// each of those cases, just without the fallback.
    async fn listed_context_window(&self, mut model: ModelInfo) -> ModelInfo {
        if let Some(provider) = self.runtime.get_provider(Some(&model.provider))
            && let Ok(listed_models) = provider.list_models().await
            && let Some(listed) = listed_models
                .into_iter()
                .find(|listed| listed.id == model.id)
        {
            model.context_window = listed.context_window;
        }
        model
    }

    fn build_agent(&self, model_override: Option<ModelInfo>) -> Result<Agent, RuntimeError> {
        let mut hidden_tools = self.hidden_tools.clone();
        hidden_tools.insert(RuntimeIntrinsicTool::Task.to_string());

        let mut config = self.config.clone();
        config.system = Some(build_subagent_system_prompt(
            self.config.system.as_deref().map(Cow::Borrowed),
        ));

        let (model, context_window, provider) = match model_override {
            Some(model) => {
                let provider = self
                    .runtime
                    .get_provider(Some(&model.provider))
                    .ok_or_else(|| RuntimeError::ProviderNotFound(Some(model.provider.clone())))?;
                (model.id, model.context_window, provider)
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

    /// Forwards to [`DisposableSubagentTemplate::spawn_from`] after verifying
    /// the template's source (see
    /// [`DisposableSubagentTemplate::verify_source`]).
    pub(crate) async fn spawn_subagent_from(
        &self,
        template: DisposableSubagentTemplate,
    ) -> Result<Self, RuntimeError> {
        template.verify_source(&self.id, &self.runtime)?;
        template.spawn_from().await
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
