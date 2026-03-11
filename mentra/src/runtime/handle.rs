use std::{
    path::Path,
    sync::{Arc, RwLock},
};

use crate::{
    ContentBlock,
    runtime::{
        AgentEvent, AgentSnapshot,
        background::{BackgroundNotification, BackgroundTaskManager, BackgroundTaskSummary},
        error::RuntimeError,
        team::{
            TeamDispatch, TeamManager, TeamMemberSummary, TeamMessage,
            TeamProtocolRequestSummary, TeamRequestFilter,
        },
    },
    tool::{ToolCall, ToolContext, ToolHandler, ToolRegistry, ToolSpec},
};
use std::sync::Mutex;
use tokio::sync::{broadcast, watch};

use super::skill::SkillLoader;

#[derive(Clone)]
pub struct RuntimeHandle {
    pub(crate) tool_registry: Arc<RwLock<ToolRegistry>>,
    pub(crate) skill_loader: Arc<RwLock<Option<SkillLoader>>>,
    pub(crate) background_tasks: BackgroundTaskManager,
    pub(crate) team: TeamManager,
    pub(crate) runtime_intrinsics_enabled: bool,
}

impl RuntimeHandle {
    pub fn new() -> Self {
        Self {
            tool_registry: Arc::new(RwLock::new(ToolRegistry::default())),
            skill_loader: Arc::new(RwLock::new(None)),
            background_tasks: BackgroundTaskManager::default(),
            team: TeamManager::default(),
            runtime_intrinsics_enabled: true,
        }
    }

    pub fn new_empty() -> Self {
        Self {
            tool_registry: Arc::new(RwLock::new(ToolRegistry::new_empty())),
            skill_loader: Arc::new(RwLock::new(None)),
            background_tasks: BackgroundTaskManager::default(),
            team: TeamManager::default(),
            runtime_intrinsics_enabled: false,
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
        team_dir: &Path,
        events: broadcast::Sender<AgentEvent>,
        snapshot_tx: watch::Sender<AgentSnapshot>,
        snapshot: Arc<Mutex<AgentSnapshot>>,
    ) -> Result<(), RuntimeError> {
        self.background_tasks
            .register_agent(agent_id, events.clone(), snapshot_tx.clone(), Arc::clone(&snapshot));
        self.team
            .register_agent(agent_name, team_dir, events, snapshot_tx, snapshot)?;
        Ok(())
    }

    pub fn start_background_task(&self, agent_id: &str, command: String) -> BackgroundTaskSummary {
        self.background_tasks.start_task(agent_id, command)
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
