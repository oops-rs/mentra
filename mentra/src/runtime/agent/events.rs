use std::path::PathBuf;

use crate::{
    ContentBlock, Message,
    runtime::{BackgroundTaskSummary, TaskItem, TeamMemberSummary, TeamProtocolRequestSummary},
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextCompactionTrigger {
    Auto,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextCompactionDetails {
    pub trigger: ContextCompactionTrigger,
    pub transcript_path: PathBuf,
    pub replaced_messages: usize,
    pub preserved_messages: usize,
    pub resulting_history_len: usize,
}

#[derive(Debug, Clone, Default)]
pub struct AgentSnapshot {
    pub status: AgentStatus,
    pub history_len: usize,
    pub current_text: String,
    pub pending_tool_uses: Vec<PendingToolUseSummary>,
    pub tasks: Vec<TaskItem>,
    pub subagents: Vec<SpawnedAgentSummary>,
    pub teammates: Vec<TeamMemberSummary>,
    pub protocol_requests: Vec<TeamProtocolRequestSummary>,
    pub background_tasks: Vec<BackgroundTaskSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    RunStarted,
    ContextCompacted {
        details: ContextCompactionDetails,
    },
    SubagentSpawned {
        agent: SpawnedAgentSummary,
    },
    SubagentFinished {
        agent: SpawnedAgentSummary,
    },
    TeammateSpawned {
        teammate: TeamMemberSummary,
    },
    TeammateUpdated {
        teammate: TeamMemberSummary,
    },
    TeamProtocolRequested {
        request: TeamProtocolRequestSummary,
    },
    TeamProtocolResolved {
        request: TeamProtocolRequestSummary,
    },
    BackgroundTaskStarted {
        task: BackgroundTaskSummary,
    },
    BackgroundTaskFinished {
        task: BackgroundTaskSummary,
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
