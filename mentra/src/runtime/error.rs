use std::{
    error::Error,
    fmt::{self, Display, Formatter},
};

use crate::provider::{ProviderError, ProviderId};

/// Errors produced while configuring, running, or recovering Mentra agents.
#[derive(Debug)]
pub enum RuntimeError {
    ProviderNotFound(Option<ProviderId>),
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
    InvalidTask(String),
    InvalidTeam(String),
    OperationDenied(String),
    Store(String),
    LeaseUnavailable(String),
    Cancelled,
    DeadlineExceeded,
    ToolBudgetExceeded(usize),
    ModelBudgetExceeded(usize),
    MaxRoundsExceeded(usize),
    InvalidToolUseInput {
        id: String,
        name: String,
        source: serde_json::Error,
    },
    MalformedProviderEvent(String),
}

impl Display for RuntimeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProviderNotFound(Some(provider)) => {
                write!(f, "provider '{provider}' is not registered")
            }
            Self::ProviderNotFound(None) => f.write_str("no providers are registered"),
            Self::FailedToSendRequest(error) => {
                write!(f, "failed to send provider request: {error}")
            }
            Self::FailedToListModels(error) => write!(f, "failed to list provider models: {error}"),
            Self::FailedToStreamResponse(error) => {
                write!(f, "failed to stream provider response: {error}")
            }
            Self::FailedToCompactHistory(error) => write!(f, "failed to compact history: {error}"),
            Self::FailedToPersistTranscript(error) => {
                write!(f, "failed to persist transcript: {error}")
            }
            Self::FailedToSerializeTranscript(error) => {
                write!(f, "failed to serialize transcript: {error}")
            }
            Self::FailedToLoadTasks(error) => write!(f, "failed to load tasks: {error}"),
            Self::FailedToWriteTasks(error) => write!(f, "failed to write tasks: {error}"),
            Self::FailedToSerializeTasks(error) => {
                write!(f, "failed to serialize tasks: {error}")
            }
            Self::FailedToRestoreTasks(error) => write!(f, "failed to restore tasks: {error}"),
            Self::FailedToLoadTeam(error) => write!(f, "failed to load team state: {error}"),
            Self::FailedToWriteTeam(error) => write!(f, "failed to write team state: {error}"),
            Self::FailedToSerializeTeam(error) => {
                write!(f, "failed to serialize team state: {error}")
            }
            Self::FailedToDeserializeTeam(error) => {
                write!(f, "failed to deserialize team state: {error}")
            }
            Self::InvalidTask(message) => write!(f, "invalid task state: {message}"),
            Self::InvalidTeam(message) => write!(f, "invalid team state: {message}"),
            Self::OperationDenied(message) => write!(f, "operation denied: {message}"),
            Self::Store(message) => write!(f, "runtime store error: {message}"),
            Self::LeaseUnavailable(message) => write!(f, "lease unavailable: {message}"),
            Self::Cancelled => f.write_str("operation cancelled"),
            Self::DeadlineExceeded => f.write_str("deadline exceeded"),
            Self::ToolBudgetExceeded(limit) => write!(f, "tool budget exceeded at {limit} call(s)"),
            Self::ModelBudgetExceeded(limit) => {
                write!(f, "model budget exceeded at {limit} request(s)")
            }
            Self::MaxRoundsExceeded(limit) => write!(f, "max rounds exceeded at {limit}"),
            Self::InvalidToolUseInput { id, name, source } => {
                write!(f, "invalid tool input for '{name}' ({id}): {source}")
            }
            Self::MalformedProviderEvent(message) => {
                write!(f, "malformed provider event: {message}")
            }
        }
    }
}

impl Error for RuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::FailedToSendRequest(error)
            | Self::FailedToListModels(error)
            | Self::FailedToStreamResponse(error)
            | Self::FailedToCompactHistory(error) => Some(error),
            Self::FailedToPersistTranscript(error)
            | Self::FailedToLoadTasks(error)
            | Self::FailedToWriteTasks(error)
            | Self::FailedToRestoreTasks(error)
            | Self::FailedToLoadTeam(error)
            | Self::FailedToWriteTeam(error) => Some(error),
            Self::FailedToSerializeTranscript(error)
            | Self::FailedToSerializeTasks(error)
            | Self::FailedToSerializeTeam(error)
            | Self::FailedToDeserializeTeam(error) => Some(error),
            Self::InvalidToolUseInput { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::RuntimeError;
    use crate::provider::ProviderId;

    #[test]
    fn display_mentions_missing_provider() {
        let error = RuntimeError::ProviderNotFound(Some(ProviderId::new("custom")));

        assert_eq!(error.to_string(), "provider 'custom' is not registered");
    }

    #[test]
    fn source_is_exposed_for_wrapped_errors() {
        let error = RuntimeError::FailedToSerializeTasks(
            serde_json::from_str::<serde_json::Value>("{").expect_err("invalid json"),
        );

        assert!(error.source().is_some());
    }
}
