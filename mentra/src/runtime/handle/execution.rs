use crate::runtime::{CommandEvaluation, Decision, ShellRequest, TaskIntrinsicTool};

use super::*;

impl RuntimeHandle {
    fn evaluate_shell_request(
        &self,
        agent_id: &str,
        tool_name: &str,
        request: ShellRequest,
    ) -> Result<(AgentExecutionConfig, CommandEvaluation, CommandRequest), String> {
        let config = self.agent_config(agent_id)?;
        let evaluation = match self
            .execution
            .policy
            .evaluate_shell_request(&config.base_dir, &request)
        {
            Ok(evaluation) => evaluation,
            Err(detail) => {
                let _ = self.emit_hook(RuntimeHookEvent::AuthorizationDenied {
                    agent_id: agent_id.to_string(),
                    action: if request.background {
                        "background_command".to_string()
                    } else {
                        "shell_command".to_string()
                    },
                    detail: detail.clone(),
                });
                return Err(detail);
            }
        };

        match evaluation.decision {
            Decision::Allow => {}
            Decision::Forbidden => {
                let detail = format!("Command forbidden by runtime policy: {}", request.command);
                let _ = self.emit_hook(RuntimeHookEvent::AuthorizationDenied {
                    agent_id: agent_id.to_string(),
                    action: if request.background {
                        "background_command".to_string()
                    } else {
                        "shell_command".to_string()
                    },
                    detail: detail.clone(),
                });
                return Err(detail);
            }
            Decision::Prompt => {
                let _ = self.emit_hook(RuntimeHookEvent::ShellApprovalRequired {
                    agent_id: agent_id.to_string(),
                    tool_name: tool_name.to_string(),
                    command: request.command.clone(),
                    cwd: request.cwd.clone(),
                    parsed_kind: evaluation.parsed.kind().to_string(),
                    matched_rules: evaluation.matched_rules.clone(),
                    justification: request.justification.clone(),
                });
                return Err(format!("Command requires approval: {}", request.command));
            }
        }

        let command_request = CommandRequest {
            spec: CommandSpec::Shell {
                command: request.command.clone(),
            },
            cwd: request.cwd,
            timeout: self
                .execution
                .policy
                .effective_timeout(request.requested_timeout),
            env: self.execution.policy.allowed_environment(),
            max_output_bytes_per_stream: self.execution.policy.max_output_bytes_per_stream,
        };

        Ok((config, evaluation, command_request))
    }

    pub fn start_background_task(
        &self,
        agent_id: &str,
        command: String,
        justification: Option<String>,
        requested_timeout: Option<Duration>,
        cwd: PathBuf,
    ) -> Result<BackgroundTaskSummary, String> {
        let (_config, _evaluation, command_request) = self.evaluate_shell_request(
            agent_id,
            "background_run",
            ShellRequest {
                command,
                cwd,
                requested_timeout,
                justification,
                background: true,
            },
        )?;

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

    pub async fn execute_shell_command(
        &self,
        agent_id: &str,
        command: String,
        justification: Option<String>,
        requested_timeout: Option<Duration>,
        cwd: PathBuf,
    ) -> Result<CommandOutput, String> {
        let (_config, _evaluation, command_request) = self.evaluate_shell_request(
            agent_id,
            "shell",
            ShellRequest {
                command,
                cwd,
                requested_timeout,
                justification,
                background: false,
            },
        )?;

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

    pub fn emit_hook(&self, event: RuntimeHookEvent) -> Result<(), RuntimeError> {
        self.execution
            .hooks
            .emit(self.persistence.store.as_ref(), &event)
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
