use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use crate::{
    Message,
    error::RuntimeError,
    runtime::RuntimeStore,
    transcript::{AgentTranscript, TranscriptItem, transcript_item_from_message},
};

use super::{
    recovery::RecoveryOutcome,
    snapshot::AgentSnapshotMemoryView,
    state::{AgentMemoryState, PendingTurnState, RunMemoryState},
    store::AgentMemoryStore,
};

#[derive(Debug, Clone)]
pub(crate) struct CompactionOutcome {
    pub transcript_path: PathBuf,
    pub transcript: AgentTranscript,
}

pub(crate) struct AgentMemory {
    agent_id: String,
    store: Arc<dyn RuntimeStore>,
    state: AgentMemoryState,
    history_cache: Vec<Message>,
}

impl AgentMemory {
    pub fn new(
        agent_id: impl Into<String>,
        store: Arc<dyn RuntimeStore>,
        state: AgentMemoryState,
    ) -> Self {
        let history_cache = state.transcript.to_messages();
        Self {
            agent_id: agent_id.into(),
            store,
            state,
            history_cache,
        }
    }

    pub fn begin_run(&mut self, run_id: String, user_message: Message) -> Result<(), RuntimeError> {
        self.state.run = Some(RunMemoryState {
            run_id,
            baseline_transcript: self.state.transcript.clone(),
            assistant_committed: false,
        });
        self.state.pending_turn = None;
        self.state.resumable_user_message = Some(user_message.clone());
        self.state
            .transcript
            .push(transcript_item_from_message(user_message));
        self.sync_history_cache();
        self.persist()
    }

    pub fn append_message(&mut self, message: Message) -> Result<(), RuntimeError> {
        self.append_transcript_item(transcript_item_from_message(message))
    }

    /// Additive counterpart to [`Self::append_message`] that also attaches
    /// opaque per-call host metadata (keyed by `tool_use_id`) to the
    /// resulting transcript item, so it survives persistence and replay
    /// without mentra interpreting it (ADR-0001 §4).
    pub fn append_message_with_details(
        &mut self,
        message: Message,
        details: BTreeMap<String, serde_json::Value>,
    ) -> Result<(), RuntimeError> {
        self.append_transcript_item(transcript_item_from_message(message).with_details(details))
    }

    pub fn append_transcript_item(&mut self, item: TranscriptItem) -> Result<(), RuntimeError> {
        self.state.transcript.push(item);
        self.sync_history_cache();
        self.persist()
    }

    pub fn update_pending_turn(&mut self, pending: PendingTurnState) -> Result<(), RuntimeError> {
        self.state.pending_turn = Some(pending);
        self.persist()
    }

    pub fn clear_pending_turn(&mut self) -> Result<(), RuntimeError> {
        self.state.pending_turn = None;
        self.persist()
    }

    pub fn commit_assistant_message(&mut self, message: Message) -> Result<(), RuntimeError> {
        self.state
            .transcript
            .push(transcript_item_from_message(message));
        self.sync_history_cache();
        self.state.pending_turn = None;
        if let Some(run) = &mut self.state.run {
            run.assistant_committed = true;
        }
        self.persist()
    }

    #[cfg(test)]
    pub fn compact(&mut self, outcome: CompactionOutcome) -> Result<(), RuntimeError> {
        self.state.transcript = outcome.transcript;
        self.sync_history_cache();
        let _ = outcome.transcript_path;
        self.persist()
    }

    pub fn rollback_failed_run(&mut self) -> Result<(), RuntimeError> {
        if let Some(run) = self.state.run.take() {
            self.state.transcript = run.baseline_transcript;
        }
        self.sync_history_cache();
        self.state.pending_turn = None;
        self.persist()
    }

    pub fn finish_run(&mut self) -> Result<(), RuntimeError> {
        self.state.pending_turn = None;
        self.state.run = None;
        self.state.resumable_user_message = None;
        self.persist()
    }

    pub fn recover(&mut self) -> Result<RecoveryOutcome, RuntimeError> {
        let Some(run) = self.state.run.take() else {
            return Ok(RecoveryOutcome::default());
        };

        let had_pending_turn = self.state.pending_turn.take().is_some();
        if had_pending_turn || !run.assistant_committed {
            self.state.transcript = run.baseline_transcript;
            self.sync_history_cache();
        } else {
            self.state.resumable_user_message = None;
        }

        self.persist()?;
        Ok(RecoveryOutcome {
            interrupted: true,
            interrupted_run_id: Some(run.run_id),
        })
    }

    pub fn transcript(&self) -> &AgentTranscript {
        &self.state.transcript
    }

    pub fn history(&self) -> &[Message] {
        &self.history_cache
    }

    pub fn revision(&self) -> u64 {
        self.state.revision
    }

    pub fn last_message(&self) -> Option<&Message> {
        self.history_cache.last()
    }

    pub fn resumable_user_message(&self) -> Option<&Message> {
        self.state.resumable_user_message.as_ref()
    }

    pub fn snapshot_view(&self) -> AgentSnapshotMemoryView {
        AgentSnapshotMemoryView::from(&self.state)
    }

    pub fn state(&self) -> &AgentMemoryState {
        &self.state
    }

    pub fn current_run_delta(&self) -> Option<Vec<Message>> {
        let run = self.state.run.as_ref()?;
        let start = run.baseline_transcript.len();
        if start >= self.state.transcript.len() {
            return Some(self.history_cache.clone());
        }
        Some(self.state.transcript.projected_messages_from(start))
    }

    pub fn try_apply_compaction(
        &mut self,
        base_revision: u64,
        outcome: CompactionOutcome,
    ) -> Result<bool, RuntimeError> {
        if self.state.revision != base_revision {
            return Ok(false);
        }
        self.state.transcript = outcome.transcript;
        self.sync_history_cache();
        let _ = outcome.transcript_path;
        self.persist()?;
        Ok(true)
    }

    fn sync_history_cache(&mut self) {
        self.history_cache = self.state.transcript.to_messages();
    }

    fn persist(&mut self) -> Result<(), RuntimeError> {
        self.state.revision = self.state.revision.saturating_add(1);
        self.store.save_memory(&self.agent_id, &self.state)
    }
}

impl PendingTurnState {
    pub fn new(
        current_text: String,
        pending_tool_uses: Vec<crate::agent::PendingToolUseSummary>,
    ) -> Self {
        Self {
            current_text,
            pending_tool_uses,
        }
    }
}
