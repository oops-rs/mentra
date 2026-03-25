use thiserror::Error;

/// Errors returned by provider implementations and stream adapters.
#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider transport error: {0}")]
    Transport(#[source] reqwest::Error),
    #[error("provider does not support capability: {0}")]
    UnsupportedCapability(String),
    #[error("{message}", message = provider_http_error(.status, .body))]
    Http {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("failed to decode provider response: {0}")]
    Decode(#[source] reqwest::Error),
    #[error("failed to serialize provider request: {0}")]
    Serialize(#[source] serde_json::Error),
    #[error("failed to deserialize provider payload: {0}")]
    Deserialize(#[source] serde_json::Error),
    #[error("invalid provider request: {0}")]
    InvalidRequest(String),
    #[error("invalid provider response: {0}")]
    InvalidResponse(String),
    #[error("malformed provider stream: {0}")]
    MalformedStream(String),
}

fn provider_http_error(status: &reqwest::StatusCode, body: &str) -> String {
    if body.trim().is_empty() {
        format!("provider returned HTTP {status}")
    } else {
        format!("provider returned HTTP {status}: {body}")
    }
}
