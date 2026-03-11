use crate::provider::{ModelProviderKind, ProviderError};

#[derive(Debug)]
pub enum RuntimeError {
    ProviderNotFound(Option<ModelProviderKind>),
    FailedToSendRequest(ProviderError),
    FailedToListModels(ProviderError),
    FailedToStreamResponse(ProviderError),
    FailedToCompactHistory(ProviderError),
    FailedToPersistTranscript(std::io::Error),
    FailedToSerializeTranscript(serde_json::Error),
    FailedToLoadTasks(std::io::Error),
    FailedToWriteTasks(std::io::Error),
    FailedToSerializeTasks(serde_json::Error),
    FailedToRestoreTasks(std::io::Error),
    FailedToLoadTeam(std::io::Error),
    FailedToWriteTeam(std::io::Error),
    FailedToSerializeTeam(serde_json::Error),
    FailedToDeserializeTeam(serde_json::Error),
    InvalidTaskGraph(String),
    InvalidTeam(String),
    MaxRoundsExceeded(usize),
    InvalidToolUseInput {
        id: String,
        name: String,
        source: serde_json::Error,
    },
    MalformedProviderEvent(String),
}
