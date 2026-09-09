use serde::{Deserialize, Serialize};

use crate::{Message, agent::PendingToolUseSummary, transcript::AgentTranscript};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentMemoryState {
    #[serde(default, deserialize_with = "deserialize_transcript")]
    pub transcript: AgentTranscript,
    pub pending_turn: Option<PendingTurnState>,
    pub resumable_user_message: Option<Message>,
    pub revision: u64,
    pub run: Option<RunMemoryState>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PendingTurnState {
    pub current_text: String,
    pub pending_tool_uses: Vec<PendingToolUseSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunMemoryState {
    pub run_id: String,
    #[serde(default, deserialize_with = "deserialize_transcript")]
    pub baseline_transcript: AgentTranscript,
    pub assistant_committed: bool,
}

fn deserialize_transcript<'de, D>(deserializer: D) -> Result<AgentTranscript, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum TranscriptRepr {
        Transcript(AgentTranscript),
        Legacy(Vec<Message>),
    }

    Ok(match TranscriptRepr::deserialize(deserializer)? {
        TranscriptRepr::Transcript(transcript) => transcript,
        TranscriptRepr::Legacy(messages) => AgentTranscript::from_messages(messages),
    })
}
