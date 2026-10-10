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
    /// The committed transcript ends inside a turn a crash interrupted after
    /// the model had already acted on it: tool calls ran (their unknown
    /// outcomes closed by recovery) and the model has not answered since.
    /// Such a turn is continued from the transcript's end, never replayed
    /// from its user message. Cleared once any run finishes.
    ///
    /// Written only when set, and absent from state older versions wrote,
    /// which reads as `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interrupted_turn: bool,
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
