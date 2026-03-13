use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{Message, agent::PendingToolUseSummary};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentMemoryState {
    pub transcript: Vec<Message>,
    pub pending_turn: Option<PendingTurnState>,
    pub resumable_user_message: Option<Message>,
    pub compaction: CompactionState,
    pub revision: u64,
    pub run: Option<RunMemoryState>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PendingTurnState {
    pub current_text: String,
    pub pending_tool_uses: Vec<PendingToolUseSummary>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompactionState {
    pub last_compacted_transcript_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunMemoryState {
    pub run_id: String,
    pub baseline_transcript: Vec<Message>,
    pub assistant_committed: bool,
}
