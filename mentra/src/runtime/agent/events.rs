use crate::{
    provider::model::{ContentBlock, Message},
    runtime::TodoItem,
    tool::ToolCall,
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AgentStatus {
    #[default]
    Idle,
    AwaitingModel,
    Streaming,
    ExecutingTool {
        id: String,
        name: String,
    },
    Finished,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingToolUseSummary {
    pub id: String,
    pub name: String,
    pub input_json: String,
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnedAgentStatus {
    Running,
    Finished,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnedAgentSummary {
    pub id: String,
    pub name: String,
    pub model: String,
    pub status: SpawnedAgentStatus,
}

#[derive(Debug, Clone, Default)]
pub struct AgentSnapshot {
    pub status: AgentStatus,
    pub history_len: usize,
    pub current_text: String,
    pub pending_tool_uses: Vec<PendingToolUseSummary>,
    pub todos: Vec<TodoItem>,
    pub subagents: Vec<SpawnedAgentSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    RunStarted,
    SubagentSpawned {
        agent: SpawnedAgentSummary,
    },
    SubagentFinished {
        agent: SpawnedAgentSummary,
    },
    TextDelta {
        delta: String,
        full_text: String,
    },
    ToolUseUpdated {
        index: usize,
        id: String,
        name: String,
        input_json: String,
    },
    ToolUseReady {
        index: usize,
        call: ToolCall,
    },
    ToolExecutionStarted {
        call: ToolCall,
    },
    ToolExecutionFinished {
        result: ContentBlock,
    },
    AssistantMessageCommitted {
        message: Message,
    },
    RunFinished,
    RunFailed {
        error: String,
    },
}
