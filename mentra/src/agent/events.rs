use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{
    BackgroundTaskSummary, ContentBlock, Message, TeamMemberSummary, TeamProtocolRequestSummary,
    compaction::CompactionExecutionMode, runtime::TaskItem, tool::ToolCall,
};

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AgentStatus {
    #[default]
    Idle,
    AwaitingModel,
    Streaming,
    ExecutingTool {
        id: String,
        name: String,
    },
    Interrupted,
    Finished,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingToolUseSummary {
    pub id: String,
    pub name: String,
    pub input_json: String,
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpawnedAgentStatus {
    Running,
    Finished,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnedAgentSummary {
    pub id: String,
    pub name: String,
    pub model: String,
    pub status: SpawnedAgentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompactionTrigger {
    Auto,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionDetails {
    pub trigger: CompactionTrigger,
    pub mode: CompactionExecutionMode,
    pub agent_id: String,
    pub transcript_path: PathBuf,
    pub replaced_items: usize,
    pub preserved_items: usize,
    pub preserved_user_turns: usize,
    pub preserved_delegation_results: usize,
    pub resulting_transcript_len: usize,
    pub extracted_facts_count: usize,
    pub summary_preview: String,
}

pub type ContextCompactionTrigger = CompactionTrigger;
pub type ContextCompactionDetails = CompactionDetails;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentSnapshot {
    pub status: AgentStatus,
    /// Monotonic generation of the run currently reflected by this snapshot.
    /// Incremented when a new `Agent::run` checkpoint has started.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub run_generation: u64,
    pub history_len: usize,
    pub current_text: String,
    pub pending_tool_uses: Vec<PendingToolUseSummary>,
    pub pending_team_messages: usize,
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
        details: CompactionDetails,
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
    TeamInboxUpdated {
        unread_count: usize,
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
    ReasoningDelta {
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
    /// Token usage from a completed model response.
    UsageReport {
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
    },
    RunFinished,
    ToolExecutionProgress {
        id: String,
        name: String,
        progress: String,
    },
    RetryAttempt {
        agent_id: String,
        error_message: String,
        attempt: u32,
        max_attempts: u32,
        next_delay_ms: u64,
    },
    RunFailed {
        error: String,
    },
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::AgentSnapshot;

    #[test]
    fn zero_run_generation_uses_the_pre_field_json_shape() {
        let json = serde_json::to_value(AgentSnapshot::default()).expect("serialize snapshot");
        assert!(json.get("run_generation").is_none());

        let restored: AgentSnapshot = serde_json::from_value(json).expect("load old snapshot JSON");
        assert_eq!(restored.run_generation, 0);

        let current = AgentSnapshot {
            run_generation: 1,
            ..AgentSnapshot::default()
        };
        let Value::Object(current) = serde_json::to_value(current).expect("serialize generation")
        else {
            panic!("snapshot must serialize as an object");
        };
        assert_eq!(current.get("run_generation"), Some(&Value::from(1)));
    }
}
