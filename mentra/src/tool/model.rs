use async_trait::async_trait;
use serde_json::Value;

use crate::runtime::BackgroundTaskSummary;
use crate::runtime::RuntimeHandle;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Clone)]
pub struct ToolContext {
    pub agent_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub(crate) runtime: RuntimeHandle,
}

impl ToolContext {
    pub fn load_skill(&self, name: &str) -> Result<String, String> {
        self.runtime.load_skill(name)
    }

    pub fn skill_descriptions(&self) -> Option<String> {
        self.runtime.skill_descriptions()
    }

    pub fn start_background_task(&self, command: String) -> BackgroundTaskSummary {
        self.runtime.start_background_task(&self.agent_id, command)
    }

    pub fn check_background_task(&self, task_id: Option<&str>) -> Result<String, String> {
        self.runtime.check_background_task(&self.agent_id, task_id)
    }
}

pub type ToolResult = Result<String, String>;

#[async_trait]
pub trait ToolHandler: Send + Sync {
    fn spec(&self) -> ToolSpec;

    async fn invoke(&self, ctx: ToolContext, input: Value) -> ToolResult;
}
