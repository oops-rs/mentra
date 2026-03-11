use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
};

use crate::{
    ContentBlock,
    runtime::{
        AgentEvent, AgentSnapshot,
        background::{BackgroundNotification, BackgroundTaskManager, BackgroundTaskSummary},
        error::RuntimeError,
        execution_context::{
            self, ExecutionContextCommandOutput, ExecutionContextStatus, ExecutionContextStore,
        },
        task::{self, TaskAccess},
        team::{
            TeamDispatch, TeamManager, TeamMemberSummary, TeamMessage, TeamProtocolRequestSummary,
            TeamRequestFilter,
        },
    },
    tool::{ToolCall, ToolContext, ToolHandler, ToolRegistry, ToolSpec},
};
use tokio::sync::{broadcast, watch};

use super::skill::SkillLoader;

#[derive(Clone)]
pub struct RuntimeHandle {
    pub(crate) tool_registry: Arc<RwLock<ToolRegistry>>,
    pub(crate) skill_loader: Arc<RwLock<Option<SkillLoader>>>,
    pub(crate) background_tasks: BackgroundTaskManager,
    pub(crate) team: TeamManager,
    pub(crate) runtime_intrinsics_enabled: bool,
    pub(crate) state_lock: Arc<Mutex<()>>,
    agent_contexts: Arc<RwLock<HashMap<String, AgentExecutionConfig>>>,
}

#[derive(Clone)]
pub(crate) struct AgentObserver {
    pub(crate) events: broadcast::Sender<AgentEvent>,
    pub(crate) snapshot_tx: watch::Sender<AgentSnapshot>,
    pub(crate) snapshot: Arc<Mutex<AgentSnapshot>>,
}

#[derive(Debug, Clone)]
pub(crate) struct AgentExecutionConfig {
    pub(crate) name: String,
    pub(crate) team_dir: PathBuf,
    pub(crate) tasks_dir: PathBuf,
    pub(crate) base_dir: PathBuf,
    pub(crate) contexts_dir: PathBuf,
    pub(crate) auto_route_shell: bool,
    pub(crate) is_teammate: bool,
}

impl RuntimeHandle {
    pub fn new() -> Self {
        Self {
            tool_registry: Arc::new(RwLock::new(ToolRegistry::default())),
            skill_loader: Arc::new(RwLock::new(None)),
            background_tasks: BackgroundTaskManager::default(),
            team: TeamManager::default(),
            runtime_intrinsics_enabled: true,
            state_lock: Arc::new(Mutex::new(())),
            agent_contexts: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn new_empty() -> Self {
        Self {
            tool_registry: Arc::new(RwLock::new(ToolRegistry::new_empty())),
            skill_loader: Arc::new(RwLock::new(None)),
            background_tasks: BackgroundTaskManager::default(),
            team: TeamManager::default(),
            runtime_intrinsics_enabled: false,
            state_lock: Arc::new(Mutex::new(())),
            agent_contexts: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn register_tool<T>(&self, tool: T)
    where
        T: ToolHandler + 'static,
    {
        self.tool_registry
            .write()
            .expect("tool registry poisoned")
            .register_tool(tool);
    }

    pub fn register_skill_loader(&self, loader: SkillLoader) {
        *self.skill_loader.write().expect("skill loader poisoned") = Some(loader);
        self.tool_registry
            .write()
            .expect("tool registry poisoned")
            .register_tool(crate::tool::builtin::LoadSkillTool);
    }

    pub fn tools(&self) -> Arc<[ToolSpec]> {
        self.tool_registry
            .read()
            .expect("tool registry poisoned")
            .tools()
    }

    pub fn runtime_intrinsics_enabled(&self) -> bool {
        self.runtime_intrinsics_enabled
    }

    pub fn skill_descriptions(&self) -> Option<String> {
        self.skill_loader
            .read()
            .expect("skill loader poisoned")
            .as_ref()
            .map(SkillLoader::get_descriptions)
            .filter(|descriptions| !descriptions.is_empty())
    }

    pub fn load_skill(&self, name: &str) -> Result<String, String> {
        let skills = self.skill_loader.read().expect("skill loader poisoned");
        let Some(loader) = skills.as_ref() else {
            return Err("Skill loader is not available".to_string());
        };

        loader.get_content(name)
    }

    pub fn register_agent(
        &self,
        agent_id: &str,
        agent_name: &str,
        config: AgentExecutionConfig,
        observer: &AgentObserver,
    ) -> Result<(), RuntimeError> {
        self.background_tasks.register_agent(agent_id, observer);
        self.team.register_agent(agent_name, &config, observer)?;
        self.agent_contexts
            .write()
            .expect("agent context registry poisoned")
            .insert(agent_id.to_string(), config);
        Ok(())
    }

    pub fn start_background_task(
        &self,
        agent_id: &str,
        command: String,
        cwd: PathBuf,
    ) -> BackgroundTaskSummary {
        self.background_tasks.start_task(agent_id, command, cwd)
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
        tool_name: &str,
        input: serde_json::Value,
        dir: &Path,
        access: TaskAccess<'_>,
    ) -> Result<String, String> {
        let _guard = self.state_lock.lock().expect("state lock poisoned");
        task::execute(tool_name, input, dir, access)
    }

    pub fn execute_execution_context_mutation(
        &self,
        tool_name: &str,
        input: serde_json::Value,
        base_dir: &Path,
        contexts_dir: &Path,
        tasks_dir: &Path,
        access: TaskAccess<'_>,
    ) -> Result<ExecutionContextCommandOutput, String> {
        let _guard = self.state_lock.lock().expect("state lock poisoned");
        execution_context::execute(tool_name, input, base_dir, contexts_dir, tasks_dir, access)
    }

    pub fn resolve_working_directory(
        &self,
        agent_id: &str,
        context_id: Option<&str>,
    ) -> Result<PathBuf, String> {
        let config = self
            .agent_contexts
            .read()
            .expect("agent context registry poisoned")
            .get(agent_id)
            .cloned()
            .ok_or_else(|| format!("Unknown agent '{agent_id}'"))?;

        let store =
            ExecutionContextStore::new(config.base_dir.clone(), config.contexts_dir.clone());

        if let Some(context_id) = context_id {
            return store
                .resolve_path(context_id)
                .map(|context| context.path)
                .map_err(|error| error.to_string());
        }

        if !config.auto_route_shell {
            return Ok(config.base_dir);
        }

        let tasks = task::TaskStore::new(config.tasks_dir)
            .load_all()
            .map_err(|error| error.to_string())?;
        let owned = tasks
            .into_iter()
            .filter(|task| {
                config.is_teammate
                    && task.owner == config.name
                    && !matches!(task.status, crate::runtime::TaskStatus::Completed)
            })
            .collect::<Vec<_>>();

        if owned.is_empty() {
            return Ok(config.base_dir);
        }

        let context_ids = owned
            .iter()
            .filter_map(|task| task.execution_context_id.clone())
            .collect::<BTreeSet<_>>();

        if context_ids.is_empty() {
            return Err(
                "You own unfinished task(s) but none has a bound execution context. Call context_create first."
                    .to_string(),
            );
        }

        if context_ids.len() > 1 {
            return Err(
                "Multiple owned execution contexts are active. Pass contextId explicitly."
                    .to_string(),
            );
        }

        let context_id = context_ids.into_iter().next().expect("one context id");
        let context = store
            .resolve_path(&context_id)
            .map_err(|error| error.to_string())?;
        match context.status {
            ExecutionContextStatus::Active | ExecutionContextStatus::Kept => Ok(context.path),
            ExecutionContextStatus::Removed => Err(format!(
                "Execution context '{}' has been removed",
                context.name
            )),
        }
    }

    pub fn default_working_directory(&self, agent_id: &str) -> PathBuf {
        self.agent_contexts
            .read()
            .expect("agent context registry poisoned")
            .get(agent_id)
            .map(|config| config.base_dir.clone())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    pub async fn execute_tool(&self, agent_id: &str, tool_call: ToolCall) -> ContentBlock {
        let tool = self
            .tool_registry
            .read()
            .expect("tool registry poisoned")
            .get_tool(&tool_call.name);

        if let Some(tool) = tool {
            match tool
                .invoke(
                    ToolContext {
                        agent_id: agent_id.to_string(),
                        tool_call_id: tool_call.id.clone(),
                        tool_name: tool_call.name.clone(),
                        working_directory: self
                            .resolve_working_directory(agent_id, None)
                            .unwrap_or_else(|_| self.default_working_directory(agent_id)),
                        runtime: self.clone(),
                    },
                    tool_call.input,
                )
                .await
            {
                Ok(content) => ContentBlock::ToolResult {
                    tool_use_id: tool_call.id,
                    content,
                    is_error: false,
                },
                Err(content) => ContentBlock::ToolResult {
                    tool_use_id: tool_call.id,
                    content,
                    is_error: true,
                },
            }
        } else {
            ContentBlock::ToolResult {
                tool_use_id: tool_call.id,
                content: "Tool not found".to_string(),
                is_error: true,
            }
        }
    }
}

impl Default for RuntimeHandle {
    fn default() -> Self {
        Self::new()
    }
}
