//! Errors from the A2A client.

use std::time::Duration;

use a2a_lf::error_code;

use super::config::{A2aConfigError, A2aUrlError};

/// Errors from the A2A client.
///
/// No variant produced from a server response carries a response body, a
/// JSON-RPC error message, or any other server text, and no variant carries a
/// configured header value. Server text is never interpolated into an error,
/// because a malicious agent would otherwise be able to write arbitrary
/// content — forged log lines, terminal escape sequences, instructions — into
/// an operator's logs or a model's context. Fixed metadata such as an HTTP
/// status or a JSON-RPC code remains available. This is the rule the MCP
/// clients follow, for the same reason.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum A2aError {
    #[error("invalid A2A agent configuration: {0}")]
    Config(#[from] A2aConfigError),

    /// No connection was made, so no request reached the agent.
    #[error("could not connect to the A2A agent")]
    Connect,

    #[error("the A2A agent exchange failed: {0}")]
    Transport(&'static str),

    #[error("the A2A agent answered {operation} with HTTP {status}")]
    HttpStatus {
        operation: &'static str,
        status: reqwest::StatusCode,
    },

    #[error(
        "the A2A agent answered with a redirect, which is not followed because it would send \
         credentials to an unvalidated origin"
    )]
    RedirectRefused,

    #[error("the A2A agent's reply exceeded the {limit} byte limit")]
    ResponseTooLarge { limit: usize },

    #[error("timed out after {0:?} waiting for the A2A agent")]
    Timeout(Duration),

    /// One event in a stream outgrew the bound before it ended.
    #[error("an event in the A2A agent's stream exceeded the {limit} byte limit")]
    EventTooLarge { limit: usize },

    /// A stream sent nothing at all — not even a keep-alive — for this long.
    #[error("the A2A agent's event stream sent nothing for {0:?}")]
    StreamIdle(Duration),

    /// A streaming method was answered with something other than an event
    /// stream that is not an error either.
    #[error("the A2A agent answered {method} without an event stream")]
    NotAStream { method: &'static str },

    /// The card did not parse, or parsed into something no client can use.
    #[error("the A2A agent card is invalid: {0}")]
    InvalidCard(&'static str),

    /// The card names no interface this client speaks.
    #[error(
        "the A2A agent card offers no JSONRPC or HTTP+JSON interface at protocol version 1.x, \
         which are the only bindings and version this client speaks"
    )]
    NoCompatibleInterface,

    /// The interface URL the card names is not one this client will use.
    #[error("the A2A agent card's interface URL is unusable: {0}")]
    InvalidEndpoint(A2aUrlError),

    /// The card names a plaintext interface URL and headers are configured.
    ///
    /// The interface URL is chosen by the remote side, so it is held to the
    /// same rule as the operator's own URL rather than trusted.
    #[error(
        "refusing to send configured headers to the A2A agent's interface over plaintext \
         http; the card must name https or a loopback host, or set \
         allow_plaintext_credentials"
    )]
    PlaintextEndpoint,

    /// A push config carrying a token or authentication would be sent to a
    /// plaintext interface. Those are credentials the agent presents to a
    /// webhook, and are held to the same rule as configured headers.
    #[error(
        "refusing to send a push config's token or authentication to the A2A agent's interface \
         over plaintext http; the card must name https or a loopback host, or set \
         allow_plaintext_credentials"
    )]
    PlaintextPushCredentials,

    /// An id a request would name in a path is empty, `.` or `..`, which a
    /// path cannot carry as a segment of its own. Nothing was sent.
    #[error("{method} names an id that is empty, `.` or `..`")]
    InvalidId { method: &'static str },

    #[error("the A2A agent's reply to {method} could not be decoded")]
    Decode { method: &'static str },

    /// The reply carried a different JSON-RPC id than the request.
    #[error("the A2A agent answered {method} with a reply for a different request id")]
    MismatchedReplyId { method: &'static str },

    /// The reply was well-formed but answers a different question, such as a
    /// `GetTask` returning another task.
    #[error("the A2A agent answered {method} about a different task")]
    MismatchedTask { method: &'static str },

    /// The agent answered with an A2A error: a JSON-RPC error code, or the
    /// HTTP+JSON `ErrorInfo` reason, which names the same errors.
    #[error("the A2A agent answered {method} with {error}")]
    Rpc {
        method: &'static str,
        error: A2aRpcError,
    },

    /// An extension URI to activate is empty, or holds a comma, whitespace,
    /// or a character a header cannot carry. Nothing was sent.
    #[error(
        "an A2A extension URI is empty or contains a comma, whitespace, or a character an HTTP \
         header cannot carry"
    )]
    InvalidExtensionUri,

    /// The message may have reached the agent, but no answer arrived.
    ///
    /// A `SendMessage` that fails in transit, times out, or is answered with a
    /// server error proves nothing about whether the agent accepted it first,
    /// and this client cannot tell that apart from the message never arriving.
    /// Callers must not resend automatically: the remote agent may already be
    /// working on it.
    #[error(
        "the A2A agent may have received the message but never answered; it may already be \
         working on it, so it must not be resent automatically"
    )]
    SendIndeterminate,
}

/// An error an A2A agent answered with, by its JSON-RPC code.
///
/// Over HTTP+JSON the same errors are named by an `ErrorInfo` reason, which
/// is classified into the code JSON-RPC gives it. The server's message is not
/// kept; see [`A2aError`] for why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum A2aRpcError {
    #[error("TaskNotFoundError (-32001)")]
    TaskNotFound,
    #[error("TaskNotCancelableError (-32002)")]
    TaskNotCancelable,
    #[error("PushNotificationNotSupportedError (-32003)")]
    PushNotificationNotSupported,
    #[error("UnsupportedOperationError (-32004)")]
    UnsupportedOperation,
    #[error("ContentTypeNotSupportedError (-32005)")]
    ContentTypeNotSupported,
    #[error("InvalidAgentResponseError (-32006)")]
    InvalidAgentResponse,
    #[error("ExtendedAgentCardNotConfiguredError (-32007)")]
    ExtendedAgentCardNotConfigured,
    #[error("ExtensionSupportRequiredError (-32008)")]
    ExtensionSupportRequired,
    #[error("VersionNotSupportedError (-32009)")]
    VersionNotSupported,
    #[error("JSON parse error (-32700)")]
    ParseError,
    #[error("invalid request (-32600)")]
    InvalidRequest,
    #[error("method not found (-32601)")]
    MethodNotFound,
    #[error("invalid params (-32602)")]
    InvalidParams,
    #[error("internal error (-32603)")]
    InternalError,
    /// A code neither A2A nor JSON-RPC defines.
    #[error("JSON-RPC error {0}")]
    Other(i32),
}

impl A2aRpcError {
    /// Classifies a JSON-RPC error code.
    pub fn from_code(code: i32) -> Self {
        match code {
            error_code::TASK_NOT_FOUND => Self::TaskNotFound,
            error_code::TASK_NOT_CANCELABLE => Self::TaskNotCancelable,
            error_code::PUSH_NOTIFICATION_NOT_SUPPORTED => Self::PushNotificationNotSupported,
            error_code::UNSUPPORTED_OPERATION => Self::UnsupportedOperation,
            error_code::CONTENT_TYPE_NOT_SUPPORTED => Self::ContentTypeNotSupported,
            error_code::INVALID_AGENT_RESPONSE => Self::InvalidAgentResponse,
            error_code::EXTENDED_CARD_NOT_CONFIGURED => Self::ExtendedAgentCardNotConfigured,
            error_code::EXTENSION_SUPPORT_REQUIRED => Self::ExtensionSupportRequired,
            error_code::VERSION_NOT_SUPPORTED => Self::VersionNotSupported,
            error_code::PARSE_ERROR => Self::ParseError,
            error_code::INVALID_REQUEST => Self::InvalidRequest,
            error_code::METHOD_NOT_FOUND => Self::MethodNotFound,
            error_code::INVALID_PARAMS => Self::InvalidParams,
            error_code::INTERNAL_ERROR => Self::InternalError,
            other => Self::Other(other),
        }
    }

    /// The JSON-RPC code this error was classified from.
    pub fn code(self) -> i32 {
        match self {
            Self::TaskNotFound => error_code::TASK_NOT_FOUND,
            Self::TaskNotCancelable => error_code::TASK_NOT_CANCELABLE,
            Self::PushNotificationNotSupported => error_code::PUSH_NOTIFICATION_NOT_SUPPORTED,
            Self::UnsupportedOperation => error_code::UNSUPPORTED_OPERATION,
            Self::ContentTypeNotSupported => error_code::CONTENT_TYPE_NOT_SUPPORTED,
            Self::InvalidAgentResponse => error_code::INVALID_AGENT_RESPONSE,
            Self::ExtendedAgentCardNotConfigured => error_code::EXTENDED_CARD_NOT_CONFIGURED,
            Self::ExtensionSupportRequired => error_code::EXTENSION_SUPPORT_REQUIRED,
            Self::VersionNotSupported => error_code::VERSION_NOT_SUPPORTED,
            Self::ParseError => error_code::PARSE_ERROR,
            Self::InvalidRequest => error_code::INVALID_REQUEST,
            Self::MethodNotFound => error_code::METHOD_NOT_FOUND,
            Self::InvalidParams => error_code::INVALID_PARAMS,
            Self::InternalError => error_code::INTERNAL_ERROR,
            Self::Other(code) => code,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_round_trips() {
        for code in (-32009..=-32001).chain([-32700, -32600, -32601, -32602, -32603, -1, 7]) {
            assert_eq!(A2aRpcError::from_code(code).code(), code);
        }
        assert_eq!(A2aRpcError::from_code(-32001), A2aRpcError::TaskNotFound);
        assert_eq!(A2aRpcError::from_code(42), A2aRpcError::Other(42));
    }
}
