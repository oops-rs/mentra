use std::{
    any::Any,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::{
    ContextCompactionDetails, ContextCompactionTrigger, DisposableSubagentTemplate,
    SpawnedAgentStatus, SpawnedAgentSummary,
};

use crate::runtime::{RuntimeError, TaskIntrinsicTool, TaskItem};
use crate::team::{TeamDispatch, TeamMemberSummary, TeamMessage, TeamProtocolRequestSummary};
use crate::tool::ToolAuthorizationPreview;

/// High-level capability labels used for tool metadata and policy decisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolCapability {
    ReadOnly,
    FilesystemRead,
    FilesystemWrite,
    ProcessExec,
    BackgroundExec,
    TaskMutation,
    TeamCoordination,
    Delegation,
    ContextCompaction,
    SkillLoad,
    Custom(String),
}

/// Declares how much side effect a tool may have when executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ToolSideEffectLevel {
    #[default]
    None,
    LocalState,
    Process,
    External,
}

/// Declares whether a tool call is safe to replay or persist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ToolDurability {
    #[default]
    Ephemeral,
    Persistent,
    ReplaySafe,
}

/// Declares whether a tool call may execute concurrently with other calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ToolExecutionMode {
    #[default]
    Exclusive,
    Parallel,
}

/// Provider-facing description of a tool and its input schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub capabilities: Vec<ToolCapability>,
    pub side_effect_level: ToolSideEffectLevel,
    pub durability: ToolDurability,
    /// Optional runtime-enforced timeout for the tool implementation itself.
    pub execution_timeout: Option<Duration>,
}

impl ToolSpec {
    /// Starts building a [`ToolSpec`] from the required tool name.
    pub fn builder(name: impl Into<String>) -> ToolSpecBuilder {
        ToolSpecBuilder {
            name: name.into(),
            description: None,
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
            capabilities: Vec::new(),
            side_effect_level: ToolSideEffectLevel::None,
            durability: ToolDurability::Ephemeral,
            execution_timeout: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolSpecBuilder {
    name: String,
    description: Option<String>,
    input_schema: Value,
    capabilities: Vec<ToolCapability>,
    side_effect_level: ToolSideEffectLevel,
    durability: ToolDurability,
    execution_timeout: Option<Duration>,
}

impl ToolSpecBuilder {
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn input_schema(mut self, input_schema: Value) -> Self {
        self.input_schema = input_schema;
        self
    }

    pub fn capability(mut self, capability: ToolCapability) -> Self {
        self.capabilities.push(capability);
        self
    }

    pub fn capabilities(mut self, capabilities: impl IntoIterator<Item = ToolCapability>) -> Self {
        self.capabilities = capabilities.into_iter().collect();
        self
    }

    pub fn side_effect_level(mut self, side_effect_level: ToolSideEffectLevel) -> Self {
        self.side_effect_level = side_effect_level;
        self
    }

    pub fn durability(mut self, durability: ToolDurability) -> Self {
        self.durability = durability;
        self
    }

    pub fn execution_timeout(mut self, execution_timeout: Duration) -> Self {
        self.execution_timeout = Some(execution_timeout);
        self
    }

    pub fn build(self) -> ToolSpec {
        ToolSpec {
            name: self.name,
            description: self.description,
            input_schema: self.input_schema,
            capabilities: self.capabilities,
            side_effect_level: self.side_effect_level,
            durability: self.durability,
            execution_timeout: self.execution_timeout,
        }
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
}

impl ToolContext<'_> {
    /// Returns the resolved working directory for the current tool execution.
    pub fn working_directory(&self) -> &Path {
        self.working_directory.as_path()
    }

    /// Returns the current agent's display name.
    pub fn agent_name(&self) -> &str {
        self.agent.name()
    }

    /// Returns the current model identifier.
    pub fn model(&self) -> &str {
        self.agent.model()
    }

    /// Returns the number of messages currently stored in history.
    pub fn history_len(&self) -> usize {
        self.agent.history().len()
    }

    /// Returns the current agent's task snapshot.
    pub fn tasks(&self) -> &[TaskItem] {
        self.agent.tasks()
    }

    /// Resolves an optional working-directory override.
    pub fn resolve_working_directory(
        &self,
        working_directory: Option<&str>,
    ) -> Result<PathBuf, String> {
        self.runtime
            .resolve_working_directory(&self.agent_id, working_directory)
    }

    /// Loads a named skill body from the registered skills directory.
    pub fn load_skill(&self, name: &str) -> Result<String, String> {
        self.runtime.load_skill(name)
    }

    /// Returns skill descriptions exposed to the model, when available.
    pub fn skill_descriptions(&self) -> Option<String> {
        self.runtime.skill_descriptions()
    }

    /// Returns typed application state registered on the runtime.
    pub fn app_context<T>(&self) -> Result<Arc<T>, String>
    where
        T: Any + Send + Sync + 'static,
    {
        self.runtime.app_context::<T>()
    }

    /// Executes a foreground shell command through the runtime policy and executor.
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

    /// Starts a background shell command through the runtime policy and executor.
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

    /// Reads the status of one or more background tasks.
    pub fn check_background_task(&self, task_id: Option<&str>) -> Result<String, String> {
        self.runtime.check_background_task(&self.agent_id, task_id)
    }

    /// Requests that the current agent yield back to its idle loop.
    pub fn request_idle(&mut self) {
        self.agent.request_idle();
    }

    /// Runs the builtin context-compaction flow immediately.
    pub async fn compact_history(
        &mut self,
    ) -> Result<Option<ContextCompactionDetails>, RuntimeError> {
        self.agent
            .compact_history(
                self.agent.history().len().saturating_sub(1),
                ContextCompactionTrigger::Manual,
            )
            .await
    }

    /// Executes one of the persisted task tools directly.
    pub fn execute_task_tool(
        &self,
        tool: &TaskIntrinsicTool,
        input: Value,
    ) -> Result<String, String> {
        self.agent.execute_task_mutation(tool, input)
    }

    /// Reloads persisted task state into the current agent snapshot.
    pub fn refresh_tasks(&mut self) -> Result<(), RuntimeError> {
        self.agent.refresh_tasks_from_disk()
    }

    /// Reads a file through the runtime's read policy.
    pub async fn read_file(&self, path: &str, max_lines: Option<usize>) -> Result<String, String> {
        self.runtime
            .read_file(&self.agent_id, path, max_lines)
            .await
    }

    /// Spawns a disposable subagent that inherits the current runtime.
    pub fn spawn_subagent(&self) -> Result<crate::agent::Agent, RuntimeError> {
        self.agent.spawn_subagent()
    }

    /// Records a subagent in the current agent snapshot.
    pub fn register_subagent(&mut self, agent: &crate::agent::Agent) -> SpawnedAgentSummary {
        self.agent.register_subagent(agent)
    }

    /// Marks a tracked subagent as finished.
    pub fn finish_subagent(
        &mut self,
        id: &str,
        status: SpawnedAgentStatus,
    ) -> Option<SpawnedAgentSummary> {
        self.agent.finish_subagent(id, status)
    }

    /// Spawns a persistent teammate in the current team namespace.
    pub async fn spawn_teammate(
        &mut self,
        name: impl Into<String>,
        role: impl Into<String>,
        prompt: Option<String>,
    ) -> Result<TeamMemberSummary, RuntimeError> {
        self.agent.spawn_teammate(name, role, prompt).await
    }

    /// Sends a direct message to a teammate.
    pub fn send_team_message(
        &self,
        to: &str,
        content: impl Into<String>,
    ) -> Result<TeamDispatch, RuntimeError> {
        self.agent.send_team_message(to, content)
    }

    /// Broadcasts a message to every known teammate except the sender.
    pub fn broadcast_team_message(
        &self,
        content: impl Into<String>,
    ) -> Result<Vec<TeamDispatch>, RuntimeError> {
        self.agent.broadcast_team_message(content)
    }

    /// Reads and acknowledges the current team inbox.
    pub fn read_team_inbox(&self) -> Result<Vec<TeamMessage>, RuntimeError> {
        self.agent.read_team_inbox()
    }

    /// Creates a team protocol request addressed to a teammate.
    pub fn request_team_protocol(
        &self,
        to: &str,
        protocol: impl Into<String>,
        content: impl Into<String>,
    ) -> Result<TeamProtocolRequestSummary, RuntimeError> {
        self.agent.request_team_protocol(to, protocol, content)
    }

    /// Resolves a team protocol request.
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
        }
    }
}

impl ParallelToolContext {
    /// Returns the resolved working directory for the current tool execution.
    pub fn working_directory(&self) -> &Path {
        self.working_directory.as_path()
    }

    /// Returns the current agent's display name.
    pub fn agent_name(&self) -> &str {
        &self.agent_name
    }

    /// Returns the current model identifier.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Returns the number of messages currently stored in history.
    pub fn history_len(&self) -> usize {
        self.history_len
    }

    /// Returns the current agent's task snapshot.
    pub fn tasks(&self) -> &[TaskItem] {
        &self.tasks
    }

    /// Resolves an optional working-directory override.
    pub fn resolve_working_directory(
        &self,
        working_directory: Option<&str>,
    ) -> Result<PathBuf, String> {
        self.runtime
            .resolve_working_directory(&self.agent_id, working_directory)
    }

    /// Loads a named skill body from the registered skills directory.
    pub fn load_skill(&self, name: &str) -> Result<String, String> {
        self.runtime.load_skill(name)
    }

    /// Returns skill descriptions exposed to the model, when available.
    pub fn skill_descriptions(&self) -> Option<String> {
        self.runtime.skill_descriptions()
    }

    /// Returns typed application state registered on the runtime.
    pub fn app_context<T>(&self) -> Result<Arc<T>, String>
    where
        T: Any + Send + Sync + 'static,
    {
        self.runtime.app_context::<T>()
    }

    /// Executes a foreground shell command through the runtime policy and executor.
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

    /// Starts a background shell command through the runtime policy and executor.
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

    /// Reads the status of one or more background tasks.
    pub fn check_background_task(&self, task_id: Option<&str>) -> Result<String, String> {
        self.runtime.check_background_task(&self.agent_id, task_id)
    }

    /// Reads a file through the runtime's read policy.
    pub async fn read_file(&self, path: &str, max_lines: Option<usize>) -> Result<String, String> {
        self.runtime
            .read_file(&self.agent_id, path, max_lines)
            .await
    }

    /// Spawns a disposable subagent that inherits the current runtime.
    pub fn spawn_subagent(&self) -> Result<crate::agent::Agent, RuntimeError> {
        self.subagent_template.spawn()
    }
}

/// String result returned by Mentra tools.
///
/// `Ok(content)` is sent back to the model as a `tool_result` block with
/// `is_error = false`.
///
/// `Err(content)` is also sent back to the model as a `tool_result` block, but
/// with `is_error = true`. Returning `Err(...)` from a tool does not by itself
/// abort the run; it lets the model inspect the failure and decide what to do
/// next.
///
/// Runs fail only when execution cannot continue at the runtime level, such as
/// when a tool panics or the runtime itself returns an execution error.
pub type ToolResult = Result<String, String>;

/// Trait implemented by custom tools exposed to models.
#[async_trait]
pub trait ExecutableTool: Send + Sync {
    /// Returns the static tool metadata used in model requests.
    fn spec(&self) -> ToolSpec;

    /// Returns structured metadata for pre-execution authorization.
    fn authorization_preview(
        &self,
        ctx: &ParallelToolContext,
        input: &Value,
    ) -> Result<ToolAuthorizationPreview, String> {
        let spec = self.spec();
        Ok(ToolAuthorizationPreview {
            working_directory: ctx.working_directory().to_path_buf(),
            capabilities: spec.capabilities,
            side_effect_level: spec.side_effect_level,
            durability: spec.durability,
            raw_input: input.clone(),
            structured_input: input.clone(),
        })
    }

    /// Declares whether this tool call may execute in parallel for the given payload.
    fn execution_mode(&self, _input: &Value) -> ToolExecutionMode {
        ToolExecutionMode::Exclusive
    }

    /// Executes the tool with a context that does not permit agent mutation.
    ///
    /// Return `Ok(content)` for successful tool output and `Err(content)` for
    /// tool-level failures you still want surfaced back to the model as an
    /// error `tool_result`.
    async fn execute(&self, _ctx: ParallelToolContext, _input: Value) -> ToolResult {
        Err(format!(
            "Tool '{}' does not support parallel execution",
            self.spec().name
        ))
    }

    /// Executes the tool with mutable access to the current agent.
    ///
    /// As with [`ExecutableTool::execute`], `Err(content)` produces an error
    /// `tool_result` visible to the model rather than aborting the run.
    async fn execute_mut(&self, ctx: ToolContext<'_>, input: Value) -> ToolResult {
        self.execute(ctx.into(), input).await
    }
}
