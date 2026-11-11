//! Errors from the A2A client.

use std::time::Duration;

use a2a_lf::error_code;
use reqwest::header::HeaderMap;
use serde::de::DeserializeOwned;

use super::config::{A2aConfigError, A2aUrlError};

/// Errors from the A2A client.
///
/// No variant's message carries a response body, a JSON-RPC error message,
/// or any other server text, and no variant carries a configured header
/// value. Server text is never interpolated into an error's `Display` or
/// `Debug`, because a malicious agent would otherwise be able to write
/// arbitrary content — forged log lines, terminal escape sequences,
/// instructions — into an operator's logs or a model's context. Fixed
/// metadata such as an HTTP status or a JSON-RPC code remains available.
/// This is the rule the MCP clients follow, for the same reason.
///
/// One variant keeps what the server sent, apart from its message:
/// [`HttpStatus`](Self::HttpStatus) holds the refused reply's headers and
/// body in an [`A2aHttpReply`], for a caller that knows whose they are — a
/// gateway of its own in front of the agent, say — and reads them on
/// purpose. Printing the error never shows them.
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

    /// A reply with a failing HTTP status that is not an A2A error: the
    /// agent's own, or one from whatever stands in front of it.
    ///
    /// The message names the operation and the status only. `reply` is what
    /// was answered, kept for a caller to read deliberately; see
    /// [`A2aHttpReply`].
    #[error("the A2A agent answered {operation} with HTTP {status}")]
    HttpStatus {
        operation: &'static str,
        status: reqwest::StatusCode,
        reply: Box<A2aHttpReply>,
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

/// The headers and body of a reply refused with a failing HTTP status, as
/// [`A2aError::HttpStatus`] keeps them.
///
/// Everything here was written by the far end. It is never part of an
/// error's `Display` or `Debug`: a caller that trusts the sender — a gateway
/// it is configured to call through, which names its refusals in a header
/// and explains them in a JSON body — reads it on purpose, and one that does
/// not leaves it alone. Text taken from it belongs in front of a person or a
/// model only when the caller would show that sender's words anyway.
///
/// The body is bounded twice: by the client's `max_response_bytes` when it
/// is read, and to [`MAX_BODY`](Self::MAX_BODY) bytes when it is kept, since
/// an explanation is short and an error value should stay small.
#[derive(Clone, Default)]
pub struct A2aHttpReply {
    headers: HeaderMap,
    body: Vec<u8>,
    truncated: bool,
}

impl A2aHttpReply {
    /// The most of a refused reply's body that is kept, in bytes.
    pub const MAX_BODY: usize = 64 * 1024;

    /// A reply with these headers and this body, the body cut to
    /// [`MAX_BODY`](Self::MAX_BODY). The client builds one for every refused
    /// reply; a caller builds one to test what it does with a refusal.
    pub fn new(headers: HeaderMap, mut body: Vec<u8>) -> Self {
        let truncated = body.len() > Self::MAX_BODY;
        body.truncate(Self::MAX_BODY);
        Self {
            headers,
            body,
            truncated,
        }
    }

    /// The reply's headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// The value of the header `name`, when the reply carried it once as
    /// visible ASCII. Names compare without regard to case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)?.to_str().ok()
    }

    /// The reply's body, cut to [`MAX_BODY`](Self::MAX_BODY) bytes.
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Whether the body was longer than what is kept.
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// The body decoded as JSON into `T`; `None` when it is not that, or
    /// was cut short.
    pub fn json<T: DeserializeOwned>(&self) -> Option<T> {
        if self.truncated {
            return None;
        }
        serde_json::from_slice(&self.body).ok()
    }
}

impl std::fmt::Debug for A2aHttpReply {
    /// Sizes only: the contents are the far end's words.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A2aHttpReply")
            .field("headers", &self.headers.len())
            .field("body_bytes", &self.body.len())
            .field("truncated", &self.truncated)
            .finish()
    }
}

#[cfg(test)]
mod reply_tests {
    use reqwest::header::{HeaderMap, HeaderValue};
    use serde_json::{Value, json};

    use super::A2aHttpReply;

    #[test]
    fn a_reply_reads_its_header_and_its_json_body() {
        let mut headers = HeaderMap::new();
        headers.insert("dial-error", HeaderValue::from_static("workflow-denied"));
        let reply = A2aHttpReply::new(headers, br#"{"message":"closed"}"#.to_vec());

        assert_eq!(reply.header("Dial-Error"), Some("workflow-denied"));
        assert_eq!(reply.header("absent"), None);
        assert_eq!(reply.json::<Value>(), Some(json!({"message": "closed"})));
        assert!(!reply.is_truncated());
    }

    #[test]
    fn a_long_body_is_cut_and_then_is_not_json() {
        let body = format!("\"{}\"", "x".repeat(A2aHttpReply::MAX_BODY));
        let reply = A2aHttpReply::new(HeaderMap::new(), body.into_bytes());

        assert_eq!(reply.body().len(), A2aHttpReply::MAX_BODY);
        assert!(reply.is_truncated());
        assert_eq!(reply.json::<Value>(), None);
    }

    #[test]
    fn debug_shows_sizes_and_none_of_what_was_sent() {
        let mut headers = HeaderMap::new();
        headers.insert("x-note", HeaderValue::from_static("server words"));
        let reply = A2aHttpReply::new(headers, b"server body".to_vec());

        let shown = format!("{reply:?}");
        assert!(!shown.contains("server"), "{shown}");
        assert!(shown.contains("body_bytes: 11"), "{shown}");
    }
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
