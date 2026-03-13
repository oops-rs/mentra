use crate::agent::PendingToolUseSummary;

use super::state::AgentMemoryState;

#[derive(Debug, Clone, Default)]
pub struct AgentSnapshotMemoryView {
    pub history_len: usize,
    pub current_text: String,
    pub pending_tool_uses: Vec<PendingToolUseSummary>,
}

impl From<&AgentMemoryState> for AgentSnapshotMemoryView {
    fn from(state: &AgentMemoryState) -> Self {
        Self {
            history_len: state.transcript.len(),
            current_text: state
                .pending_turn
                .as_ref()
                .map(|pending| pending.current_text.clone())
                .unwrap_or_default(),
            pending_tool_uses: state
                .pending_turn
                .as_ref()
                .map(|pending| pending.pending_tool_uses.clone())
                .unwrap_or_default(),
        }
    }
}
