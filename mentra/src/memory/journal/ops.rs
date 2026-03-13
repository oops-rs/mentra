use std::{path::PathBuf, sync::Arc};

use crate::{ContentBlock, Message, Role, error::RuntimeError, runtime::RuntimeStore};

use super::{
    recovery::RecoveryOutcome,
    snapshot::AgentSnapshotMemoryView,
    state::{AgentMemoryState, PendingTurnState, RunMemoryState},
    store::AgentMemoryStore,
};

#[derive(Debug, Clone)]
pub(crate) struct CompactionOutcome {
    pub transcript_path: PathBuf,
    pub transcript: Vec<Message>,
}

pub(crate) struct AgentMemory {
    agent_id: String,
    store: Arc<dyn RuntimeStore>,
    state: AgentMemoryState,
}

impl AgentMemory {
    pub fn new(
        agent_id: impl Into<String>,
        store: Arc<dyn RuntimeStore>,
        state: AgentMemoryState,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            store,
            state,
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
        self.state.transcript.push(user_message);
        self.persist()
    }

    pub fn append_message(&mut self, message: Message) -> Result<(), RuntimeError> {
        self.state.transcript.push(message);
        self.persist()
    }

    pub fn update_pending_turn(&mut self, pending: PendingTurnState) -> Result<(), RuntimeError> {
        self.state.pending_turn = Some(pending);
        self.persist()
    }

    pub fn commit_assistant_message(&mut self, message: Message) -> Result<(), RuntimeError> {
        self.state.transcript.push(message);
        self.state.pending_turn = None;
        if let Some(run) = &mut self.state.run {
            run.assistant_committed = true;
        }
        self.persist()
    }

    pub fn append_tool_results(&mut self, results: Vec<ContentBlock>) -> Result<(), RuntimeError> {
        self.state.transcript.push(Message {
            role: Role::User,
            content: results,
        });
        self.state.pending_turn = None;
        self.persist()
    }

    #[allow(dead_code)]
    pub fn compact(&mut self, outcome: CompactionOutcome) -> Result<(), RuntimeError> {
        self.state.transcript = outcome.transcript;
        self.state.compaction.last_compacted_transcript_path = Some(outcome.transcript_path);
        self.persist()
    }

    pub fn rollback_failed_run(&mut self) -> Result<(), RuntimeError> {
        if let Some(run) = self.state.run.take() {
            self.state.transcript = run.baseline_transcript;
        }
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
        } else {
            self.state.resumable_user_message = None;
        }

        self.persist()?;
        Ok(RecoveryOutcome {
            interrupted: true,
            interrupted_run_id: Some(run.run_id),
        })
    }

    pub fn transcript(&self) -> &[Message] {
        &self.state.transcript
    }

    pub fn revision(&self) -> u64 {
        self.state.revision
    }

    pub fn last_message(&self) -> Option<&Message> {
        self.state.transcript.last()
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
            return Some(self.state.transcript.clone());
        }
        Some(self.state.transcript[start..].to_vec())
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
        self.state.compaction.last_compacted_transcript_path = Some(outcome.transcript_path);
        self.persist()?;
        Ok(true)
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
