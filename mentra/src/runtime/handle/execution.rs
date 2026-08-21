use crate::runtime::TaskIntrinsicTool;

use super::*;

impl RuntimeHandle {
    /// Authorizes, validates and shapes one command into a request.
    ///
    /// `target` rides through untouched: it names the executor the host wants,
    /// and every check above it — working-root authorization, shell
    /// validation, the timeout clamp, the output cap, the environment
    /// allowlist — applies to a targeted command exactly as it does to a local
    /// one. A target chooses where an authorized command runs; it never
    /// decides whether it may.
    fn build_command_request(
        &self,
        agent_id: &str,
        target: Option<String>,
        command: String,
        requested_timeout: Option<Duration>,
        cwd: PathBuf,
        background: bool,
    ) -> Result<(AgentExecutionConfig, CommandRequest), String> {
        let config = self.agent_config(agent_id)?;
        if let Err(detail) =
            self.execution
                .policy
                .authorize_command_execution(&config.base_dir, &cwd, background)
        {
            let _ = self.emit_hook(RuntimeHookEvent::AuthorizationDenied {
                agent_id: agent_id.to_string(),
                action: if background {
                    "background_command".to_string()
                } else {
                    "shell_command".to_string()
                },
                detail: detail.clone(),
            });
            return Err(detail);
        }

        let validation = self
            .execution
            .policy
            .evaluate_shell_command(&command, &config.base_dir);
        if validation.should_emit_hook() {
            let detail = validation
                .reason()
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| "Shell command requires validation".to_string());
            let _ = self.emit_hook(RuntimeHookEvent::AuthorizationDenied {
                agent_id: agent_id.to_string(),
                action: if background {
                    "background_shell_validation".to_string()
                } else {
                    "shell_validation".to_string()
                },
                detail: detail.clone(),
            });
            if validation.should_deny() {
                return Err(detail);
            }
        }

        let command_request = CommandRequest {
            spec: CommandSpec::Shell { command },
            cwd,
            timeout: self.execution.policy.effective_timeout(requested_timeout),
            env: self.execution.policy.allowed_environment(),
            max_output_bytes_per_stream: self.execution.policy.max_output_bytes_per_stream,
            target,
        };

        Ok((config, command_request))
    }

    pub fn start_background_task(
        &self,
        agent_id: &str,
        command: String,
        _justification: Option<String>,
        requested_timeout: Option<Duration>,
        cwd: PathBuf,
    ) -> Result<BackgroundTaskSummary, String> {
        // Background tasks are untargeted in this release: a task outlives the
        // call that started it, and nothing yet reports a remote task's fate
        // back to the agent that asked for it.
        let (_config, command_request) =
            self.build_command_request(agent_id, None, command, requested_timeout, cwd, true)?;

        if let Some(limit) = self.execution.policy.background_task_limit
            && self
                .collaboration
                .background_tasks
                .running_task_count(agent_id)
                >= limit
        {
            let detail = format!("Background task limit of {limit} reached");
            let _ = self.emit_hook(RuntimeHookEvent::AuthorizationDenied {
                agent_id: agent_id.to_string(),
                action: "background_limit".to_string(),
                detail: detail.clone(),
            });
            return Err(detail);
        }

        self.collaboration
            .background_tasks
            .start_task(agent_id, command_request)
    }

    pub fn check_background_task(
        &self,
        agent_id: &str,
        task_id: Option<&str>,
    ) -> Result<String, String> {
        self.collaboration
            .background_tasks
            .check_task(agent_id, task_id)
    }

    pub fn drain_background_notifications(&self, agent_id: &str) -> Vec<BackgroundNotification> {
        self.collaboration
            .background_tasks
            .drain_notifications(agent_id)
    }

    pub fn has_deliverable_background_notifications(&self, agent_id: &str) -> bool {
        self.collaboration
            .background_tasks
            .has_deliverable_notifications(agent_id)
    }

    pub fn requeue_background_notifications(
        &self,
        agent_id: &str,
        notifications: Vec<BackgroundNotification>,
    ) {
        self.collaboration
            .background_tasks
            .requeue_notifications(agent_id, notifications);
    }

    pub fn acknowledge_background_notifications(&self, agent_id: &str) {
        self.collaboration
            .background_tasks
            .acknowledge_notifications(agent_id);
    }

    pub fn spawn_teammate_actor(
        &self,
        team_dir: &Path,
        teammate_name: &str,
        agent: std::sync::Arc<tokio::sync::Mutex<crate::Agent>>,
    ) -> Result<crate::team::TeammateActorHandle, RuntimeError> {
        Ok(self.collaboration.teammate_host.spawn_teammate(
            self.collaboration.team.clone(),
            team_dir.to_path_buf(),
            teammate_name.to_string(),
            agent,
        ))
    }

    pub fn register_teammate(
        &self,
        team_dir: &Path,
        summary: TeamMemberSummary,
        actor: crate::team::TeammateActorHandle,
    ) -> Result<TeamMemberSummary, RuntimeError> {
        self.collaboration
            .team
            .spawn_teammate(team_dir, summary, actor)
    }

    pub fn wake_teammate(&self, team_dir: &Path, teammate_name: &str) -> Result<(), RuntimeError> {
        self.collaboration
            .team
            .wake_teammate(team_dir, teammate_name)
    }

    pub fn send_team_message(
        &self,
        team_dir: &Path,
        sender: &str,
        to: &str,
        content: String,
    ) -> Result<TeamDispatch, RuntimeError> {
        self.collaboration
            .team
            .send_message(team_dir, sender, to, content)
    }

    pub fn broadcast_team_message(
        &self,
        team_dir: &Path,
        sender: &str,
        content: String,
    ) -> Result<Vec<TeamDispatch>, RuntimeError> {
        self.collaboration
            .team
            .broadcast_message(team_dir, sender, content)
    }

    pub fn read_team_inbox(
        &self,
        team_dir: &Path,
        agent_name: &str,
    ) -> Result<Vec<TeamMessage>, RuntimeError> {
        self.collaboration.team.read_inbox(team_dir, agent_name)
    }

    pub fn requeue_team_messages(
        &self,
        team_dir: &Path,
        agent_name: &str,
        messages: Vec<TeamMessage>,
    ) -> Result<(), RuntimeError> {
        self.collaboration
            .team
            .requeue_messages(team_dir, agent_name, messages)
    }

    pub fn acknowledge_team_messages(
        &self,
        team_dir: &Path,
        agent_name: &str,
    ) -> Result<(), RuntimeError> {
        self.collaboration
            .team
            .acknowledge_messages(team_dir, agent_name)
    }

    pub fn create_team_request(
        &self,
        team_dir: &Path,
        sender: &str,
        to: &str,
        protocol: String,
        content: String,
    ) -> Result<TeamProtocolRequestSummary, RuntimeError> {
        self.collaboration
            .team
            .create_request(team_dir, sender, to, protocol, content)
    }

    pub fn resolve_team_request(
        &self,
        team_dir: &Path,
        responder: &str,
        request_id: &str,
        approve: bool,
        reason: Option<String>,
    ) -> Result<TeamProtocolRequestSummary, RuntimeError> {
        self.collaboration
            .team
            .resolve_request(team_dir, responder, request_id, approve, reason)
    }

    pub fn list_team_requests(
        &self,
        team_dir: &Path,
        agent_name: &str,
        filter: TeamRequestFilter,
    ) -> Result<Vec<TeamProtocolRequestSummary>, RuntimeError> {
        self.collaboration
            .team
            .list_requests(team_dir, agent_name, filter)
    }

    pub fn execute_task_mutation(
        &self,
        tool: &TaskIntrinsicTool,
        input: serde_json::Value,
        dir: &Path,
        access: TaskAccess<'_>,
    ) -> Result<String, String> {
        task::execute_with_store(self.persistence.store.as_ref(), tool, input, dir, access)
    }

    /// Runs one command on the local executor.
    pub async fn execute_shell_command(
        &self,
        agent_id: &str,
        command: String,
        justification: Option<String>,
        requested_timeout: Option<Duration>,
        cwd: PathBuf,
    ) -> Result<CommandOutput, String> {
        self.execute_shell_command_on(
            agent_id,
            None,
            command,
            justification,
            requested_timeout,
            cwd,
        )
        .await
    }

    /// Runs one command on the executor the host named.
    ///
    /// `target` is passed to the installed [`RuntimeExecutor`] on the request
    /// and is not interpreted here: which names exist, and what each one
    /// reaches, is the host's business. Every guard around the command is the
    /// same one an untargeted call gets. `None` means the local executor;
    /// the builtin [`LocalRuntimeExecutor`] refuses any other name rather than
    /// running a command that was addressed elsewhere.
    pub async fn execute_shell_command_on(
        &self,
        agent_id: &str,
        target: Option<String>,
        command: String,
        _justification: Option<String>,
        requested_timeout: Option<Duration>,
        cwd: PathBuf,
    ) -> Result<CommandOutput, String> {
        let (_config, command_request) =
            self.build_command_request(agent_id, target, command, requested_timeout, cwd, false)?;

        self.execution.executor.run(command_request).await
    }

    pub async fn read_file(
        &self,
        agent_id: &str,
        path: &str,
        max_lines: Option<usize>,
    ) -> Result<String, String> {
        let config = self.agent_config(agent_id)?;
        let resolved = match self
            .execution
            .policy
            .authorize_file_read(&config.base_dir, Path::new(path))
        {
            Ok(path) => path,
            Err(detail) => {
                let _ = self.emit_hook(RuntimeHookEvent::AuthorizationDenied {
                    agent_id: agent_id.to_string(),
                    action: "read_file".to_string(),
                    detail: detail.clone(),
                });
                return Err(detail);
            }
        };

        read_limited_file(&resolved, max_lines).await
    }

    pub fn resolve_working_directory(
        &self,
        agent_id: &str,
        explicit_directory: Option<&str>,
    ) -> Result<PathBuf, String> {
        let config = self.agent_config(agent_id)?;

        if let Some(directory) = explicit_directory {
            return Ok(resolve_path(&config.base_dir, directory));
        }

        if !config.auto_route_shell {
            return Ok(config.base_dir);
        }

        let tasks = self
            .persistence
            .store
            .load_tasks(&config.tasks_dir)
            .map_err(|error| error.to_string())?;
        let owned = tasks
            .into_iter()
            .filter(|task| {
                config.is_teammate
                    && task.owner == config.name
                    && !matches!(task.status, crate::runtime::TaskStatus::Completed)
            })
            .collect::<Vec<_>>();

        let directories = owned
            .iter()
            .filter_map(|task| task.working_directory.as_deref())
            .map(|path| resolve_path(&config.base_dir, path))
            .collect::<BTreeSet<_>>();

        if directories.is_empty() {
            return Ok(config.base_dir);
        }

        if directories.len() > 1 {
            return Err(
                "Multiple owned task directories are active. Pass workingDirectory explicitly."
                    .to_string(),
            );
        }

        Ok(directories.into_iter().next().expect("one directory"))
    }

    pub fn default_working_directory(&self, agent_id: &str) -> PathBuf {
        self.agent_contexts
            .read()
            .expect("agent context registry poisoned")
            .get(agent_id)
            .map(|config| config.base_dir.clone())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    pub(crate) fn shell_validation(
        &self,
        agent_id: &str,
        command: &str,
    ) -> Result<crate::runtime::control::ShellValidation, String> {
        let config = self.agent_config(agent_id)?;
        Ok(self
            .execution
            .policy
            .evaluate_shell_command(command, &config.base_dir))
    }

    pub fn emit_hook(&self, event: RuntimeHookEvent) -> Result<(), RuntimeError> {
        self.execution
            .hooks
            .emit_runtime(self.persistence.store.as_ref(), &event)
    }
}

fn resolve_path(base_dir: &Path, path: &str) -> PathBuf {
    let candidate = PathBuf::from(path);
    if candidate.is_absolute() {
        candidate
    } else {
        base_dir.join(candidate)
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::*;
    use crate::runtime::{
        VolatileRuntimeStore,
        control::{CommandOutput, LocalRuntimeExecutor, RuntimeExecutor, RuntimePolicy},
    };

    const AGENT_ID: &str = "agent-1";

    /// Records what the handle handed it and answers without running anything,
    /// so a test can read the request the routing layer actually produced.
    #[derive(Default)]
    struct RecordingExecutor {
        requests: Mutex<Vec<CommandRequest>>,
    }

    impl RecordingExecutor {
        fn last_target(&self) -> Option<String> {
            self.requests
                .lock()
                .expect("recorded requests poisoned")
                .last()
                .expect("one recorded request")
                .target
                .clone()
        }
    }

    #[async_trait]
    impl RuntimeExecutor for RecordingExecutor {
        async fn run(&self, request: CommandRequest) -> Result<CommandOutput, String> {
            self.requests
                .lock()
                .expect("recorded requests poisoned")
                .push(request);
            Ok(CommandOutput {
                stdout: "recorded".to_string(),
                stderr: String::new(),
                success: true,
                status_code: Some(0),
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
            })
        }
    }

    /// A handle wired to `executor`, with one agent registered and a policy
    /// that permits shell commands. The store is volatile so nothing here
    /// touches the machine-wide database.
    fn handle_with(executor: Arc<dyn RuntimeExecutor>) -> RuntimeHandle {
        let handle = RuntimeHandle::new(false)
            .rebind_store(Arc::new(VolatileRuntimeStore::new()))
            .with_policy(RuntimePolicy::permissive())
            .with_executor(executor);
        let base_dir = std::env::temp_dir();
        handle
            .agent_contexts
            .write()
            .expect("agent context registry poisoned")
            .insert(
                AGENT_ID.to_string(),
                AgentExecutionConfig {
                    name: "agent".to_string(),
                    team_dir: base_dir.clone(),
                    tasks_dir: base_dir.clone(),
                    base_dir,
                    memory_tool_search_limit: 5,
                    auto_route_shell: false,
                    is_teammate: false,
                },
            );
        handle
    }

    #[tokio::test]
    async fn a_named_target_reaches_the_executor() {
        let executor = Arc::new(RecordingExecutor::default());
        let handle = handle_with(executor.clone());

        handle
            .execute_shell_command_on(
                AGENT_ID,
                Some("x".to_string()),
                "true".to_string(),
                None,
                None,
                std::env::temp_dir(),
            )
            .await
            .expect("the stub executor answers");

        assert_eq!(executor.last_target(), Some("x".to_string()));
    }

    #[tokio::test]
    async fn an_untargeted_command_reaches_the_executor_with_no_target() {
        let executor = Arc::new(RecordingExecutor::default());
        let handle = handle_with(executor.clone());

        handle
            .execute_shell_command(
                AGENT_ID,
                "true".to_string(),
                None,
                None,
                std::env::temp_dir(),
            )
            .await
            .expect("the stub executor answers");

        assert_eq!(executor.last_target(), None);
    }

    /// The refusal has to come from the executor, not from a local run that
    /// happened to succeed: a command addressed to a host that this runtime
    /// cannot reach must fail loudly rather than execute here.
    #[tokio::test]
    async fn the_local_executor_refuses_a_target_it_does_not_serve() {
        let handle = handle_with(Arc::new(LocalRuntimeExecutor));

        let error = handle
            .execute_shell_command_on(
                AGENT_ID,
                Some("mac".to_string()),
                "true".to_string(),
                None,
                None,
                std::env::temp_dir(),
            )
            .await
            .expect_err("a targeted command must not run on the local executor");

        assert_eq!(
            error,
            "no executor serves target `mac`; the local executor only runs untargeted commands"
        );
    }
}
