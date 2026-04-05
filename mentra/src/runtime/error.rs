use crate::provider::{ProviderError, ProviderId};
use crate::runtime::control::is_transient_provider_error;
use thiserror::Error;

/// Classifies a [`RuntimeError`] by its recoverability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCategory {
    /// Transient failure that may succeed on retry.
    Retryable,
    /// Permanent failure that cannot be retried.
    Terminal,
    /// Operation continued but state may be inconsistent.
    Degraded,
}

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

impl RuntimeError {
    /// Returns the [`ErrorCategory`] for this error, classifying it as
    /// retryable, terminal, or degraded.
    pub fn category(&self) -> ErrorCategory {
        match self {
            // Provider-backed errors: delegate to transient check.
            Self::FailedToSendRequest(source)
            | Self::FailedToStreamResponse(source)
            | Self::FailedToCompactHistory(source) => {
                if is_transient_provider_error(source) {
                    ErrorCategory::Retryable
                } else {
                    ErrorCategory::Terminal
                }
            }

            // Listing models is not retryable even if the provider error is transient.
            Self::FailedToListModels(_) => ErrorCategory::Terminal,

            // Configuration and logic errors are permanent.
            Self::ProviderNotFound(_)
            | Self::NoModelsAvailable(_)
            | Self::OperationDenied(_)
            | Self::Cancelled
            | Self::DeadlineExceeded
            | Self::ToolBudgetExceeded(_)
            | Self::ModelBudgetExceeded(_)
            | Self::MaxRoundsExceeded(_)
            | Self::EmptyAssistantResponse
            | Self::NoResumableTurn
            | Self::InvalidToolUseInput { .. }
            | Self::MalformedProviderEvent(_)
            | Self::InvalidTask(_)
            | Self::InvalidTeam(_) => ErrorCategory::Terminal,

            // Persistence and store errors: state may be inconsistent.
            Self::FailedToPersistTranscript(_)
            | Self::FailedToSerializeTranscript(_)
            | Self::FailedToLoadTasks(_)
            | Self::FailedToWriteTasks(_)
            | Self::FailedToSerializeTasks(_)
            | Self::FailedToRestoreTasks(_)
            | Self::FailedToLoadTeam(_)
            | Self::FailedToWriteTeam(_)
            | Self::FailedToSerializeTeam(_)
            | Self::FailedToDeserializeTeam(_)
            | Self::Store(_)
            | Self::LeaseUnavailable(_) => ErrorCategory::Degraded,
        }
    }
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

    use super::{ErrorCategory, RuntimeError};
    use crate::provider::{ProviderError, ProviderId};

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

    #[test]
    fn transient_provider_error_is_retryable() {
        let error = RuntimeError::FailedToSendRequest(ProviderError::Retryable {
            message: "rate limited".into(),
            delay: None,
        });

        assert_eq!(error.category(), ErrorCategory::Retryable);
    }

    #[test]
    fn permanent_provider_error_is_terminal() {
        let error =
            RuntimeError::FailedToSendRequest(ProviderError::InvalidRequest("bad body".into()));

        assert_eq!(error.category(), ErrorCategory::Terminal);
    }

    #[test]
    fn budget_exceeded_is_terminal() {
        assert_eq!(
            RuntimeError::ToolBudgetExceeded(100).category(),
            ErrorCategory::Terminal,
        );
        assert_eq!(
            RuntimeError::ModelBudgetExceeded(50).category(),
            ErrorCategory::Terminal,
        );
    }

    #[test]
    fn persistence_io_error_is_degraded() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "disk full");
        let error = RuntimeError::FailedToPersistTranscript(io_err);

        assert_eq!(error.category(), ErrorCategory::Degraded);
    }
}
