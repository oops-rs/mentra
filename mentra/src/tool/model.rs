use std::{
    any::Any,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use mentra_provider::ToolResultContent;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::{
    CompactionDetails, CompactionTrigger, DisposableSubagentTemplate, SpawnedAgentStatus,
    SpawnedAgentSummary,
};
use crate::runtime::{RuntimeError, TaskIntrinsicTool, TaskItem};
use crate::team::{TeamDispatch, TeamMemberSummary, TeamMessage, TeamProtocolRequestSummary};
use crate::tool::ToolAuthorizationPreview;

use super::descriptor::{RuntimeToolDescriptor, ToolExecutionMode};

#[allow(unused_imports)]
pub use mentra_provider::ToolLoadingPolicy;
pub type ToolSpec = RuntimeToolDescriptor;

#[cfg(test)]
mod tests {
    use crate::tool::{ProviderToolSpec, ToolLoadingPolicy};
    use serde_json::json;

    #[test]
    fn tool_spec_builder_defaults_to_immediate_loading() {
        let spec = ProviderToolSpec::builder("echo_tool")
            .description("Echo a value.")
            .input_schema(json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string" }
                }
            }))
            .build();

        assert_eq!(spec.loading_policy, ToolLoadingPolicy::Immediate);
    }

    #[test]
    fn tool_spec_builder_supports_deferred_loading() {
        let spec = ProviderToolSpec::builder("echo_tool")
            .defer_loading(true)
            .build();

        assert_eq!(spec.loading_policy, ToolLoadingPolicy::Deferred);
    }

    #[test]
    fn tool_spec_deserialization_defaults_loading_policy() {
        let spec: ProviderToolSpec = serde_json::from_value(json!({
            "name": "echo_tool",
            "description": "Echo a value.",
            "input_schema": {
                "type": "object",
                "properties": {}
            }
        }))
        .expect("deserialize tool spec");

        assert_eq!(spec.loading_policy, ToolLoadingPolicy::Immediate);
    }
}

/// A concrete tool call emitted by a model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// Execution context made available to a running tool.
pub struct ToolContext<'a> {
    pub agent_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub(crate) working_directory: PathBuf,
    pub(crate) runtime: crate::runtime::RuntimeHandle,
    pub(crate) agent: &'a mut crate::agent::Agent,
    pub(crate) event_tx: crate::agent::AgentEventBus,
    /// The options of the `Agent::run` call this execution is a step of.
    /// Reachable only as [`child_run_options`](Self::child_run_options), so a
    /// tool can share the run's aggregate bounds with work it spawns but cannot
    /// read or edit the run's own policy.
    pub(crate) run_options: crate::runtime::RunOptions,
}

impl ToolContext<'_> {
    pub fn working_directory(&self) -> &Path {
        self.working_directory.as_path()
    }

    /// [`RunOptions`](crate::runtime::RunOptions) for a run this tool spawns,
    /// derived from the options of the run this tool is executing under — see
    /// [`RunOptions::child`](crate::runtime::RunOptions::child) for what a child
    /// inherits and what it resets.
    ///
    /// Thread these into the spawned run's own `Agent::run` call. A subagent
    /// driven on `RunOptions::default()` instead gets a fresh, unbounded token
    /// counter, so its spend escapes the parent's `token_budget` and a parent
    /// cancel, stop, or deadline never reaches it.
    pub fn child_run_options(&self) -> crate::runtime::RunOptions {
        self.run_options.child()
    }

    /// Emit a progress event for the currently executing tool.
    pub fn emit_progress(&self, progress: String) {
        self.event_tx
            .send(crate::agent::AgentEvent::ToolExecutionProgress {
                id: self.tool_call_id.clone(),
                name: self.tool_name.clone(),
                progress,
            });
    }

    pub fn agent_name(&self) -> &str {
        self.agent.name()
    }

    pub fn model(&self) -> &str {
        self.agent.model()
    }

    pub fn history_len(&self) -> usize {
        self.agent.history().len()
    }

    pub fn tasks(&self) -> &[TaskItem] {
        self.agent.tasks()
    }

    /// Returns this agent's tool-result paging configuration, if enabled.
    pub(crate) fn tool_result_paging(&self) -> Option<crate::agent::ToolResultPagingConfig> {
        self.agent.config().tool_result_paging
    }

    /// Returns the full text of one of this agent's paged tool results.
    /// Scoped to the agent by construction: the retained results live on the
    /// agent this context borrows, so no cross-agent read is expressible.
    pub(crate) fn paged_tool_result(&self, tool_use_id: &str) -> Option<Arc<str>> {
        self.agent.paged_tool_result(tool_use_id)
    }

    pub fn resolve_working_directory(
        &self,
        working_directory: Option<&str>,
    ) -> Result<PathBuf, String> {
        self.runtime
            .resolve_working_directory(&self.agent_id, working_directory)
    }

    pub fn load_skill(&self, name: &str) -> Result<String, String> {
        self.runtime.load_skill(name)
    }

    pub fn skill_descriptions(&self) -> Option<String> {
        self.runtime.skill_descriptions()
    }

    pub fn app_context<T>(&self) -> Result<Arc<T>, String>
    where
        T: Any + Send + Sync + 'static,
    {
        self.runtime.app_context::<T>()
    }

    /// Runs one command on the local executor.
    pub async fn execute_shell_command(
        &self,
        command: String,
        justification: Option<String>,
        requested_timeout: Option<std::time::Duration>,
        cwd: PathBuf,
    ) -> Result<crate::runtime::CommandOutput, String> {
        self.runtime
            .execute_shell_command(
                &self.agent_id,
                command,
                justification,
                requested_timeout,
                cwd,
            )
            .await
    }

    /// Runs one command on the executor the host named.
    ///
    /// A tool that lets its caller say *where* a command runs passes the name
    /// here; `None` is the local executor. The name reaches the installed
    /// [`crate::runtime::RuntimeExecutor`] on the request and is interpreted
    /// only there, so a tool can route a command without gaining any way to
    /// route around the policy that authorized it.
    pub async fn execute_shell_command_on(
        &self,
        target: Option<String>,
        command: String,
        justification: Option<String>,
        requested_timeout: Option<std::time::Duration>,
        cwd: PathBuf,
    ) -> Result<crate::runtime::CommandOutput, String> {
        self.runtime
            .execute_shell_command_on(
                &self.agent_id,
                target,
                command,
                justification,
                requested_timeout,
                cwd,
            )
            .await
    }

    pub fn start_background_task(
        &self,
        command: String,
        justification: Option<String>,
        requested_timeout: Option<std::time::Duration>,
        cwd: PathBuf,
    ) -> Result<crate::BackgroundTaskSummary, String> {
        self.runtime.start_background_task(
            &self.agent_id,
            command,
            justification,
            requested_timeout,
            cwd,
        )
    }

    pub fn check_background_task(&self, task_id: Option<&str>) -> Result<String, String> {
        self.runtime.check_background_task(&self.agent_id, task_id)
    }

    pub fn request_idle(&mut self) {
        self.agent.request_idle();
    }

    pub async fn compact_history(&mut self) -> Result<Option<CompactionDetails>, RuntimeError> {
        self.agent
            .compact_history(
                self.agent.history().len().saturating_sub(1),
                CompactionTrigger::Manual,
            )
            .await
    }

    pub fn execute_task_tool(
        &self,
        tool: &TaskIntrinsicTool,
        input: Value,
    ) -> Result<String, String> {
        self.agent.execute_task_mutation(tool, input)
    }

    pub fn refresh_tasks(&mut self) -> Result<(), RuntimeError> {
        self.agent.refresh_tasks_from_disk()
    }

    pub async fn read_file(&self, path: &str, max_lines: Option<usize>) -> Result<String, String> {
        self.runtime
            .read_file(&self.agent_id, path, max_lines)
            .await
    }

    pub fn spawn_subagent(&self) -> Result<crate::agent::Agent, RuntimeError> {
        self.agent.spawn_subagent()
    }

    /// Records a spawned subagent and announces it on the parent's stream.
    ///
    /// Emitting is part of registering here, where for the `task` intrinsic
    /// the two are separate calls: a tool that registered a child without
    /// announcing it left the child in the parent's snapshot and absent from
    /// every observer's view of it, and there is no reason a caller would
    /// want that. The event is the same `SubagentSpawned` the intrinsic emits.
    pub fn register_subagent(&mut self, agent: &crate::agent::Agent) -> SpawnedAgentSummary {
        let summary = self.agent.register_subagent(agent);
        self.agent
            .emit_event(crate::agent::AgentEvent::SubagentSpawned {
                agent: summary.clone(),
            });
        summary
    }

    /// Marks a subagent finished and announces it on the parent's stream.
    ///
    /// The other half of [`register_subagent`](Self::register_subagent).
    /// Returns `None` — and announces nothing — when no subagent under `id`
    /// was registered.
    pub fn finish_subagent(
        &mut self,
        id: &str,
        status: SpawnedAgentStatus,
    ) -> Option<SpawnedAgentSummary> {
        let finished = self.agent.finish_subagent(id, status)?;
        self.agent
            .emit_event(crate::agent::AgentEvent::SubagentFinished {
                agent: finished.clone(),
            });
        Some(finished)
    }

    /// Relays a child agent's token usage onto this agent's event stream.
    ///
    /// A subagent has its own event bus, so an observer watching the parent
    /// sees none of what a delegated run spent — while that spend still counts
    /// against the parent's `token_budget`. Relaying `UsageReport` keeps the
    /// parent's stream summing to the same total the accounting reports.
    ///
    /// The returned guard must outlive the child's run: dropping it stops the
    /// relay, so binding it to `_` ends it immediately.
    #[must_use = "the relay stops when this guard is dropped"]
    pub fn relay_subagent_usage(
        &self,
        child: &crate::agent::Agent,
    ) -> crate::agent::AgentEventTapGuard {
        self.relay_subagent_events(child, |event| {
            matches!(event, crate::agent::AgentEvent::UsageReport { .. })
        })
    }

    /// Relays the child agent's events that `filter` accepts onto this agent's
    /// stream.
    ///
    /// The general form of [`relay_subagent_usage`](Self::relay_subagent_usage),
    /// for a tool that wants a delegated run's tool calls or text visible to
    /// whoever is watching the parent. Relaying everything means a parent's
    /// observer sees two interleaved runs, so the filter is the parameter
    /// rather than a default.
    ///
    /// The returned guard must outlive the child's run.
    #[must_use = "the relay stops when this guard is dropped"]
    pub fn relay_subagent_events(
        &self,
        child: &crate::agent::Agent,
        filter: impl Fn(&crate::agent::AgentEvent) -> bool + Send + Sync + 'static,
    ) -> crate::agent::AgentEventTapGuard {
        let parent_events = self.agent.event_sender();
        child.register_event_tap(move |event| {
            if filter(event) {
                parent_events.send(event.clone());
            }
        })
    }

    /// Records a delegation this tool performed in the parent's transcript.
    ///
    /// Delegation entries are what a transcript reader follows to reconstruct
    /// who asked whom for what. Only the `task` intrinsic could write them, so
    /// a tool that delegated work its own way left no trace of the delegation
    /// — the result appeared in the transcript with nothing saying where it
    /// came from.
    pub fn record_delegation_request(
        &mut self,
        content: impl Into<String>,
        delegation: crate::transcript::DelegationArtifact,
        edge: Option<crate::transcript::DelegationEdge>,
    ) -> Result<(), RuntimeError> {
        self.agent
            .record_delegation_request(content, delegation, edge)?;
        self.agent.sync_memory_snapshot();
        Ok(())
    }

    /// Records the outcome of a delegation this tool performed.
    ///
    /// The other half of
    /// [`record_delegation_request`](Self::record_delegation_request): the
    /// request says what was asked and this says what came back, and a reader
    /// following the edges needs both.
    pub fn record_delegation_result(
        &mut self,
        content: impl Into<String>,
        delegation: crate::transcript::DelegationArtifact,
        edge: Option<crate::transcript::DelegationEdge>,
    ) -> Result<(), RuntimeError> {
        self.agent
            .record_delegation_result(content, delegation, edge)?;
        self.agent.sync_memory_snapshot();
        Ok(())
    }

    pub async fn spawn_teammate(
        &mut self,
        name: impl Into<String>,
        role: impl Into<String>,
        prompt: Option<String>,
    ) -> Result<TeamMemberSummary, RuntimeError> {
        self.agent.spawn_teammate(name, role, prompt).await
    }

    pub fn send_team_message(
        &self,
        to: &str,
        content: impl Into<String>,
    ) -> Result<TeamDispatch, RuntimeError> {
        self.agent.send_team_message(to, content)
    }

    pub fn broadcast_team_message(
        &self,
        content: impl Into<String>,
    ) -> Result<Vec<TeamDispatch>, RuntimeError> {
        self.agent.broadcast_team_message(content)
    }

    pub fn read_team_inbox(&self) -> Result<Vec<TeamMessage>, RuntimeError> {
        self.agent.read_team_inbox()
    }

    pub fn request_team_protocol(
        &self,
        to: &str,
        protocol: impl Into<String>,
        content: impl Into<String>,
    ) -> Result<TeamProtocolRequestSummary, RuntimeError> {
        self.agent.request_team_protocol(to, protocol, content)
    }

    pub fn respond_team_protocol(
        &self,
        request_id: &str,
        approve: bool,
        reason: Option<String>,
    ) -> Result<TeamProtocolRequestSummary, RuntimeError> {
        self.agent
            .respond_team_protocol(request_id, approve, reason)
    }
}

/// Execution context made available to a parallel-safe running tool.
#[derive(Clone)]
pub struct ParallelToolContext {
    pub agent_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub(crate) working_directory: PathBuf,
    pub(crate) runtime: crate::runtime::RuntimeHandle,
    pub(crate) subagent_template: DisposableSubagentTemplate,
    pub(crate) agent_name: String,
    pub(crate) model: String,
    pub(crate) history_len: usize,
    pub(crate) tasks: Vec<TaskItem>,
    pub(crate) event_tx: crate::agent::AgentEventBus,
    /// The options of the `Agent::run` call this execution is a step of, as on
    /// [`ToolContext::run_options`].
    pub(crate) run_options: crate::runtime::RunOptions,
}

impl From<ToolContext<'_>> for ParallelToolContext {
    fn from(ctx: ToolContext) -> Self {
        ParallelToolContext {
            agent_id: ctx.agent_id,
            tool_call_id: ctx.tool_call_id,
            tool_name: ctx.tool_name,
            working_directory: ctx.working_directory,
            runtime: ctx.runtime,
            subagent_template: ctx.agent.disposable_subagent_template(),
            agent_name: ctx.agent.name().to_string(),
            model: ctx.agent.model().to_string(),
            history_len: ctx.agent.history().len(),
            tasks: ctx.agent.tasks().to_vec(),
            event_tx: ctx.event_tx,
            run_options: ctx.run_options,
        }
    }
}

impl ParallelToolContext {
    pub fn working_directory(&self) -> &Path {
        self.working_directory.as_path()
    }

    /// Emit a progress event for the currently executing tool.
    pub fn emit_progress(&self, progress: String) {
        self.event_tx
            .send(crate::agent::AgentEvent::ToolExecutionProgress {
                id: self.tool_call_id.clone(),
                name: self.tool_name.clone(),
                progress,
            });
    }

    pub fn agent_name(&self) -> &str {
        &self.agent_name
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn history_len(&self) -> usize {
        self.history_len
    }

    pub fn tasks(&self) -> &[TaskItem] {
        &self.tasks
    }

    pub fn resolve_working_directory(
        &self,
        working_directory: Option<&str>,
    ) -> Result<PathBuf, String> {
        self.runtime
            .resolve_working_directory(&self.agent_id, working_directory)
    }

    pub(crate) fn shell_validation(
        &self,
        command: &str,
    ) -> Result<crate::runtime::control::ShellValidation, String> {
        self.runtime.shell_validation(&self.agent_id, command)
    }

    pub fn load_skill(&self, name: &str) -> Result<String, String> {
        self.runtime.load_skill(name)
    }

    pub fn skill_descriptions(&self) -> Option<String> {
        self.runtime.skill_descriptions()
    }

    pub fn app_context<T>(&self) -> Result<Arc<T>, String>
    where
        T: Any + Send + Sync + 'static,
    {
        self.runtime.app_context::<T>()
    }

    /// Runs one command on the local executor.
    pub async fn execute_shell_command(
        &self,
        command: String,
        justification: Option<String>,
        requested_timeout: Option<std::time::Duration>,
        cwd: PathBuf,
    ) -> Result<crate::runtime::CommandOutput, String> {
        self.runtime
            .execute_shell_command(
                &self.agent_id,
                command,
                justification,
                requested_timeout,
                cwd,
            )
            .await
    }

    /// Runs one command on the executor the host named.
    ///
    /// A tool that lets its caller say *where* a command runs passes the name
    /// here; `None` is the local executor. The name reaches the installed
    /// [`crate::runtime::RuntimeExecutor`] on the request and is interpreted
    /// only there, so a tool can route a command without gaining any way to
    /// route around the policy that authorized it.
    pub async fn execute_shell_command_on(
        &self,
        target: Option<String>,
        command: String,
        justification: Option<String>,
        requested_timeout: Option<std::time::Duration>,
        cwd: PathBuf,
    ) -> Result<crate::runtime::CommandOutput, String> {
        self.runtime
            .execute_shell_command_on(
                &self.agent_id,
                target,
                command,
                justification,
                requested_timeout,
                cwd,
            )
            .await
    }

    pub fn start_background_task(
        &self,
        command: String,
        justification: Option<String>,
        requested_timeout: Option<std::time::Duration>,
        cwd: PathBuf,
    ) -> Result<crate::BackgroundTaskSummary, String> {
        self.runtime.start_background_task(
            &self.agent_id,
            command,
            justification,
            requested_timeout,
            cwd,
        )
    }

    pub fn check_background_task(&self, task_id: Option<&str>) -> Result<String, String> {
        self.runtime.check_background_task(&self.agent_id, task_id)
    }

    pub async fn read_file(&self, path: &str, max_lines: Option<usize>) -> Result<String, String> {
        self.runtime
            .read_file(&self.agent_id, path, max_lines)
            .await
    }

    pub fn spawn_subagent(&self) -> Result<crate::agent::Agent, RuntimeError> {
        self.subagent_template.spawn()
    }

    /// [`RunOptions`](crate::runtime::RunOptions) for a run this tool spawns —
    /// the parallel-lane counterpart of
    /// [`ToolContext::child_run_options`], carrying the same caveat: a subagent
    /// from [`spawn_subagent`](Self::spawn_subagent) driven on
    /// `RunOptions::default()` gets a fresh, unbounded token counter and shares
    /// none of the parent run's cancellation, stop, or deadline.
    pub fn child_run_options(&self) -> crate::runtime::RunOptions {
        self.run_options.child()
    }
}

/// String result returned by Mentra tools.
pub type ToolResult = Result<String, String>;

/// Structured, additive successor to [`ToolResult`].
///
/// `content` is the provider-visible projection of a tool's result and reuses
/// the existing [`ToolResultContent`] from `mentra-provider`, so no new
/// provider representation is required. `details` is opaque host metadata
/// that survives the local transcript but is never sent to a provider — mentra
/// never interprets it. `terminate` asks the run to end as the value of this
/// tool's own execution: a first-class successor to
/// [`ToolContext::request_idle`] for terminal actions, honored only when the
/// call executes in an exclusive lane (see
/// [`RuntimeToolDescriptorBuilder::terminal`](crate::tool::RuntimeToolDescriptorBuilder::terminal)).
///
/// Tool-level failures keep using the existing `Err(String)` channel on
/// [`ToolExecutor::execute_output`] / [`ToolExecutor::execute_mut_output`];
/// `ToolOutput` only ever appears on the `Ok` side.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: ToolResultContent,
    pub details: Option<Value>,
    pub terminate: bool,
}

impl ToolOutput {
    /// Builds a plain-text, non-terminating output with no attached metadata.
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: ToolResultContent::Text(content.into()),
            details: None,
            terminate: false,
        }
    }

    /// Builds a structured, non-terminating output with no attached metadata.
    pub fn structured(content: Value) -> Self {
        Self {
            content: ToolResultContent::Structured(content),
            details: None,
            terminate: false,
        }
    }

    /// Attaches opaque host metadata that survives transcript persistence but
    /// is never projected to a provider.
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    /// Marks this output as ending the run as the value of its own execution.
    pub fn terminating(mut self) -> Self {
        self.terminate = true;
        self
    }
}

/// Bridges an existing `Ok(String)` tool result into the additive structured
/// path: `Text` content, no metadata, no termination.
impl From<String> for ToolOutput {
    fn from(value: String) -> Self {
        Self::text(value)
    }
}

/// Definition contract for custom tools exposed to models.
///
/// **Adding a method here means adding it to `tool::forwarding` too**, in both
/// the `Box` and `Arc` impls. Those forward every method explicitly, and a new
/// one with a default body would compile and pass the suite while silently
/// answering for the pointer instead of the tool inside it.
pub trait ToolDefinition: Send + Sync {
    fn descriptor(&self) -> RuntimeToolDescriptor;
}

/// Execution contract for custom tools exposed to models.
///
/// **Adding a method here means adding it to `tool::forwarding` too**, in both
/// the `Box` and `Arc` impls. Those forward every method explicitly, and a new
/// one with a default body would compile and pass the suite while silently
/// answering for the pointer instead of the tool inside it -- which for
/// [`authorization_preview`](Self::authorization_preview) means presenting a
/// host's tool to the approver as something other than what it is.
#[async_trait]
pub trait ToolExecutor: ToolDefinition + Send + Sync {
    fn authorization_preview(
        &self,
        ctx: &ParallelToolContext,
        input: &Value,
    ) -> Result<ToolAuthorizationPreview, String> {
        let descriptor = self.descriptor();
        Ok(ToolAuthorizationPreview {
            working_directory: ctx.working_directory().to_path_buf(),
            capabilities: descriptor.capabilities,
            side_effect_level: descriptor.side_effect_level,
            durability: descriptor.durability,
            execution_category: descriptor.execution_category,
            approval_category: descriptor.approval_category,
            raw_input: input.clone(),
            structured_input: input.clone(),
        })
    }

    fn execution_category(&self, _input: &Value) -> super::descriptor::ToolExecutionCategory {
        self.descriptor().execution_category
    }

    fn execution_mode(&self, input: &Value) -> ToolExecutionMode {
        self.execution_category(input).into()
    }

    async fn execute(&self, _ctx: ParallelToolContext, _input: Value) -> ToolResult {
        Err(format!(
            "Tool '{}' does not support parallel execution",
            self.descriptor().provider.name
        ))
    }

    async fn execute_mut(&self, ctx: ToolContext<'_>, input: Value) -> ToolResult {
        self.execute(ctx.into(), input).await
    }

    /// Structured, parallel-lane execution. Defaults to bridging
    /// [`ToolExecutor::execute`] through `ToolOutput::from`, so every
    /// existing string-returning tool keeps working unchanged. Overriding
    /// this directly (instead of `execute`) opts a tool into structured
    /// content, opaque details, or (subject to the exclusive-lane
    /// requirement) termination.
    async fn execute_output(
        &self,
        ctx: ParallelToolContext,
        input: Value,
    ) -> Result<ToolOutput, String> {
        self.execute(ctx, input).await.map(ToolOutput::from)
    }

    /// Structured, exclusive-lane execution. Defaults to bridging
    /// [`ToolExecutor::execute_mut`] through `ToolOutput::from`, so every
    /// existing string-returning tool keeps working unchanged. Overriding
    /// this directly (instead of `execute_mut`) opts a tool into structured
    /// content, opaque details, or termination.
    async fn execute_mut_output(
        &self,
        ctx: ToolContext<'_>,
        input: Value,
    ) -> Result<ToolOutput, String> {
        self.execute_mut(ctx, input).await.map(ToolOutput::from)
    }
}

/// Runtime tool contract used by Mentra registries and execution.
pub trait ExecutableTool: ToolDefinition + ToolExecutor {}

impl<T> ExecutableTool for T where T: ToolDefinition + ToolExecutor {}
