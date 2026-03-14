use crate::provider::{ProviderError, ProviderId};
use thiserror::Error;

/// Errors produced while configuring, running, or recovering Mentra agents.
#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("{message}", message = provider_not_found_message(.0))]
    ProviderNotFound(Option<ProviderId>),
    #[error("provider '{0}' did not return any models")]
    NoModelsAvailable(ProviderId),
    #[error("failed to send provider request: {0}")]
    FailedToSendRequest(#[source] ProviderError),
    #[error("failed to list provider models: {0}")]
    FailedToListModels(#[source] ProviderError),
    #[error("failed to stream provider response: {0}")]
    FailedToStreamResponse(#[source] ProviderError),
    #[error("failed to compact history: {0}")]
    FailedToCompactHistory(#[source] ProviderError),
    #[error("failed to persist transcript: {0}")]
    FailedToPersistTranscript(#[source] std::io::Error),
    #[error("failed to serialize transcript: {0}")]
    FailedToSerializeTranscript(#[source] serde_json::Error),
    #[error("failed to load tasks: {0}")]
    FailedToLoadTasks(#[source] std::io::Error),
    #[error("failed to write tasks: {0}")]
    FailedToWriteTasks(#[source] std::io::Error),
    #[error("failed to serialize tasks: {0}")]
    FailedToSerializeTasks(#[source] serde_json::Error),
    #[error("failed to restore tasks: {0}")]
    FailedToRestoreTasks(#[source] std::io::Error),
    #[error("failed to load team state: {0}")]
    FailedToLoadTeam(#[source] std::io::Error),
    #[error("failed to write team state: {0}")]
    FailedToWriteTeam(#[source] std::io::Error),
    #[error("failed to serialize team state: {0}")]
    FailedToSerializeTeam(#[source] serde_json::Error),
    #[error("failed to deserialize team state: {0}")]
    FailedToDeserializeTeam(#[source] serde_json::Error),
    #[error("invalid task state: {0}")]
    InvalidTask(String),
    #[error("invalid team state: {0}")]
    InvalidTeam(String),
    #[error("operation denied: {0}")]
    OperationDenied(String),
    #[error("runtime store error: {0}")]
    Store(String),
    #[error("lease unavailable: {0}")]
    LeaseUnavailable(String),
    #[error("operation cancelled")]
    Cancelled,
    #[error("deadline exceeded")]
    DeadlineExceeded,
    #[error("tool budget exceeded at {0} call(s)")]
    ToolBudgetExceeded(usize),
    #[error("model budget exceeded at {0} request(s)")]
    ModelBudgetExceeded(usize),
    #[error("max rounds exceeded at {0}")]
    MaxRoundsExceeded(usize),
    #[error("run completed without a final assistant message")]
    EmptyAssistantResponse,
    #[error("no resumable user turn is available")]
    NoResumableTurn,
    #[error("invalid tool input for '{name}' ({id}): {source}")]
    InvalidToolUseInput {
        id: String,
        name: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("malformed provider event: {0}")]
    MalformedProviderEvent(String),
}

fn provider_not_found_message(provider: &Option<ProviderId>) -> String {
    match provider {
        Some(provider) => format!("provider '{provider}' is not registered"),
        None => "no providers are registered".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::RuntimeError;
    use crate::provider::ProviderId;

    #[test]
    fn display_mentions_missing_provider() {
        let error = RuntimeError::ProviderNotFound(Some(ProviderId::new("custom")));

        assert_eq!(error.to_string(), "provider 'custom' is not registered");
    }

    #[test]
    fn display_mentions_missing_models() {
        let error = RuntimeError::NoModelsAvailable(ProviderId::new("custom"));

        assert_eq!(
            error.to_string(),
            "provider 'custom' did not return any models"
        );
    }

    #[test]
    fn source_is_exposed_for_wrapped_errors() {
        let error = RuntimeError::FailedToSerializeTasks(
            serde_json::from_str::<serde_json::Value>("{").expect_err("invalid json"),
        );

        assert!(error.source().is_some());
    }

    #[test]
    fn empty_assistant_response_has_clear_display_text() {
        assert_eq!(
            RuntimeError::EmptyAssistantResponse.to_string(),
            "run completed without a final assistant message"
        );
    }
}
