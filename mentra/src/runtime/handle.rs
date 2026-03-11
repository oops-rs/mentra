use std::{collections::HashSet, sync::Arc, sync::RwLock};

use crate::{
    provider::model::ContentBlock,
    runtime::{
        AgentEvent, AgentSnapshot,
        background::{BackgroundNotification, BackgroundTaskManager, BackgroundTaskSummary},
    },
    tool::{ToolCall, ToolContext, ToolHandler, ToolRegistry, ToolSpec},
};
use std::sync::Mutex;
use tokio::sync::{broadcast, watch};

use super::skill::SkillLoader;

#[derive(Clone, Default)]
pub struct RuntimeHandle {
    pub(crate) tool_registry: Arc<RwLock<ToolRegistry>>,
    pub(crate) skill_loader: Arc<RwLock<Option<SkillLoader>>>,
    pub(crate) background_tasks: BackgroundTaskManager,
}

impl RuntimeHandle {
    pub fn new_empty() -> Self {
        Self {
            tool_registry: Arc::new(RwLock::new(ToolRegistry::new_empty())),
            skill_loader: Arc::new(RwLock::new(None)),
            background_tasks: BackgroundTaskManager::default(),
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

    pub fn tools_excluding(&self, hidden_tools: &HashSet<String>) -> Arc<[ToolSpec]> {
        if hidden_tools.is_empty() {
            return self.tools();
        }

        self.tool_registry
            .read()
            .expect("tool registry poisoned")
            .tools()
            .iter()
            .filter(|tool| !hidden_tools.contains(&tool.name))
            .cloned()
            .collect::<Vec<_>>()
            .into()
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
        events: broadcast::Sender<AgentEvent>,
        snapshot_tx: watch::Sender<AgentSnapshot>,
        snapshot: Arc<Mutex<AgentSnapshot>>,
    ) {
        self.background_tasks
            .register_agent(agent_id, events, snapshot_tx, snapshot);
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
