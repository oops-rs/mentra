use crate::runtime::TaskIntrinsicTool;

use super::*;

impl RuntimeHandle {
    pub fn start_background_task(
        &self,
        agent_id: &str,
        command: String,
        cwd: PathBuf,
    ) -> Result<BackgroundTaskSummary, String> {
        let config = self.agent_config(agent_id)?;
        if let Err(detail) = self.policy.authorize_command(&config.base_dir, &cwd, true) {
            let _ = self.emit_hook(RuntimeHookEvent::AuthorizationDenied {
                agent_id: agent_id.to_string(),
                action: "background_command".to_string(),
                detail: detail.clone(),
            });
            return Err(detail);
        }

        if let Some(limit) = self.policy.background_task_limit
            && self.background_tasks.running_task_count(agent_id) >= limit
        {
            let detail = format!("Background task limit of {limit} reached");
            let _ = self.emit_hook(RuntimeHookEvent::AuthorizationDenied {
                agent_id: agent_id.to_string(),
                action: "background_limit".to_string(),
                detail: detail.clone(),
            });
            return Err(detail);
        }

        self.background_tasks.start_task(
            agent_id,
            CommandRequest {
                spec: CommandSpec::Shell { command },
                cwd,
                timeout: self.policy.command_timeout,
            },
        )
    }

    pub fn check_background_task(
        &self,
        agent_id: &str,
        task_id: Option<&str>,
    ) -> Result<String, String> {
        self.background_tasks.check_task(agent_id, task_id)
    }

    pub fn drain_background_notifications(&self, agent_id: &str) -> Vec<BackgroundNotification> {
        self.background_tasks.drain_notifications(agent_id)
    }

    pub fn requeue_background_notifications(
        &self,
        agent_id: &str,
        notifications: Vec<BackgroundNotification>,
    ) {
        self.background_tasks
            .requeue_notifications(agent_id, notifications);
    }

    pub fn acknowledge_background_notifications(&self, agent_id: &str) {
        self.background_tasks.acknowledge_notifications(agent_id);
    }

    pub fn team_manager(&self) -> TeamManager {
        self.team.clone()
    }

    pub fn register_teammate(
        &self,
        team_dir: &Path,
        summary: TeamMemberSummary,
        wake_tx: tokio::sync::mpsc::UnboundedSender<()>,
        task: std::thread::JoinHandle<()>,
    ) -> Result<TeamMemberSummary, RuntimeError> {
        self.team.spawn_teammate(team_dir, summary, wake_tx, task)
    }

    pub fn wake_teammate(&self, team_dir: &Path, teammate_name: &str) -> Result<(), RuntimeError> {
        self.team.wake_teammate(team_dir, teammate_name)
    }

    pub fn send_team_message(
        &self,
        team_dir: &Path,
        sender: &str,
        to: &str,
        content: String,
    ) -> Result<TeamDispatch, RuntimeError> {
        self.team.send_message(team_dir, sender, to, content)
    }

    pub fn broadcast_team_message(
        &self,
        team_dir: &Path,
        sender: &str,
        content: String,
    ) -> Result<Vec<TeamDispatch>, RuntimeError> {
        self.team.broadcast_message(team_dir, sender, content)
    }

    pub fn read_team_inbox(
        &self,
        team_dir: &Path,
        agent_name: &str,
    ) -> Result<Vec<TeamMessage>, RuntimeError> {
        self.team.read_inbox(team_dir, agent_name)
    }

    pub fn requeue_team_messages(
        &self,
        team_dir: &Path,
        agent_name: &str,
        messages: Vec<TeamMessage>,
    ) -> Result<(), RuntimeError> {
        self.team.requeue_messages(team_dir, agent_name, messages)
    }

    pub fn acknowledge_team_messages(
        &self,
        team_dir: &Path,
        agent_name: &str,
    ) -> Result<(), RuntimeError> {
        self.team.acknowledge_messages(team_dir, agent_name)
    }

    pub fn create_team_request(
        &self,
        team_dir: &Path,
        sender: &str,
        to: &str,
        protocol: String,
        content: String,
    ) -> Result<TeamProtocolRequestSummary, RuntimeError> {
        self.team
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
        self.team
            .resolve_request(team_dir, responder, request_id, approve, reason)
    }

    pub fn list_team_requests(
        &self,
        team_dir: &Path,
        agent_name: &str,
        filter: TeamRequestFilter,
    ) -> Result<Vec<TeamProtocolRequestSummary>, RuntimeError> {
        self.team.list_requests(team_dir, agent_name, filter)
    }

    pub fn execute_task_mutation(
        &self,
        tool: &TaskIntrinsicTool,
        input: serde_json::Value,
        dir: &Path,
        access: TaskAccess<'_>,
    ) -> Result<String, String> {
        task::execute_with_store(self.store.as_ref(), tool, input, dir, access)
    }

    pub async fn execute_shell_command(
        &self,
        agent_id: &str,
        command: String,
        cwd: PathBuf,
    ) -> Result<CommandOutput, String> {
        let config = self.agent_config(agent_id)?;
        if let Err(detail) = self.policy.authorize_command(&config.base_dir, &cwd, false) {
            let _ = self.emit_hook(RuntimeHookEvent::AuthorizationDenied {
                agent_id: agent_id.to_string(),
                action: "shell_command".to_string(),
                detail: detail.clone(),
            });
            return Err(detail);
        }

        self.executor
            .run(CommandRequest {
                spec: CommandSpec::Shell { command },
                cwd,
                timeout: self.policy.command_timeout,
            })
            .await
    }

    pub async fn read_file(
        &self,
        agent_id: &str,
        path: &str,
        max_lines: Option<usize>,
    ) -> Result<String, String> {
        let config = self.agent_config(agent_id)?;
        let resolved = match self
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
        self.hooks.emit(self.store.as_ref(), &event)
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
