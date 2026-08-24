//! MCP client for the Streamable HTTP transport (protocol revision 2025-03-26
//! and later).
//!
//! # The transport
//!
//! This is the transport that replaced HTTP+SSE. Current MCP servers are
//! commonly streamable-only, answering `404` on `/sse` because they never
//! implemented the older transport at all:
//!
//! | | Streamable HTTP (this module) | legacy HTTP+SSE |
//! |---|---|---|
//! | Endpoints | one URL for both | a `GET` stream plus a separate `POST` URL |
//! | POST target | the configured URL | named by the server in an `endpoint` event |
//! | Responses | in the POST response, or a stream it opens | always on the `GET` stream |
//! | Session | the `Mcp-Session-Id` header | a query parameter in the endpoint URL |
//!
//! # Lifecycle
//!
//! 1. `POST` each JSON-RPC message to the one configured endpoint, with
//!    `Accept: application/json, text/event-stream`. A server that requires
//!    Streamable HTTP answers `406` to a request missing that header, which is
//!    the failure that made streamable-only servers unreachable before this
//!    module existed.
//! 2. Read the reply from that same response — either a single
//!    `application/json` body, or a `text/event-stream` the server opens in the
//!    response and closes once the reply is on it.
//! 3. Carry the `Mcp-Session-Id` the `initialize` response assigned, if any, on
//!    every later request, and `DELETE` the endpoint on shutdown to end it.
//!
//! The handshake is `initialize`, then a `notifications/initialized`
//! notification, then a paginated `tools/list` — the same sequence as the other
//! two transports.
//!
//! # Why this client has no reader task
//!
//! The legacy transport carries every response on one long-lived stream, so its
//! client needs a background reader, a pending-request map, and a correlation
//! window in which a response can arrive before its own `POST` future resolves.
//! Here a reply arrives on the response to the request that asked for it, so
//! none of that machinery exists: a request is one `POST`, one bounded read, and
//! one answer. The id is still checked, because a reply carrying a different id
//! is answering a different question.
//!
//! # Failure behavior
//!
//! Nothing is retried and nothing is replayed. A `tools/call` whose request may
//! have reached the server but whose reply never arrived is reported as
//! [`McpStreamableHttpError::RequestIndeterminate`] rather than as a plain
//! failure, so a caller can tell "may have run" apart from "definitely did
//! not". An MCP tool can send mail, charge a card, or write a file, and this
//! client never makes that decision on a caller's behalf.

#[cfg(test)]
mod tests;

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::de::DeserializeOwned;
use serde_json::Value as JsonValue;
use url::Url;

use super::config::{
    McpStreamableHttpConfigError, McpStreamableHttpLimits, McpStreamableHttpServerConfig,
};
use crate::mcp::protocol::*;
use crate::mcp::sse::endpoint::EndpointError;
use crate::mcp::sse::wire::{SseParser, SseWireError};

/// The protocol revision this transport requests.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// The revisions this client can actually speak.
///
/// Streamable HTTP was introduced in `2025-03-26`, so a server offering this
/// transport cannot legitimately negotiate anything older. The specification
/// says a client should disconnect when the server answers with a revision it
/// does not support, and the alternative here is worse than disconnecting:
/// sending that revision back in `MCP-Protocol-Version` on every later request
/// while behaving as this client actually does.
const SUPPORTED_PROTOCOL_VERSIONS: [&str; 2] = ["2025-06-18", "2025-03-26"];

/// The `Accept` header every request carries.
///
/// Both types are offered because the server chooses per reply. A server that
/// implements only Streamable HTTP rejects a request that does not offer both.
const ACCEPT_BOTH: &str = "application/json, text/event-stream";

/// Header naming the session assigned by `initialize`.
const SESSION_HEADER: &str = "mcp-session-id";

/// Header naming the negotiated protocol revision.
const PROTOCOL_HEADER: &str = "mcp-protocol-version";

/// Bound on how much of an HTTP error body is read before discarding it.
///
/// The body is attacker-controlled and never surfaces in an error; this bound
/// exists so that reading it to return the connection to the pool cannot be
/// turned into a memory-exhaustion primitive.
const MAX_DIAGNOSTIC_BODY_BYTES: usize = 8 * 1024;

/// Bound on the length of a server-assigned session id.
///
/// The id is echoed on every later request, so an unbounded one would let a
/// server dictate the size of every request this client sends.
const MAX_SESSION_ID_BYTES: usize = 512;

/// Errors from the MCP Streamable HTTP client.
///
/// No variant produced from a server response carries a response body, an SSE
/// payload, a JSON-RPC message or data value, a tool argument, or a tool
/// result. Server text is never interpolated into an error, because a malicious
/// server would otherwise be able to write arbitrary content — including forged
/// log lines and terminal escape sequences — into an operator's logs or a
/// model's context. Fixed metadata such as an HTTP status or a JSON-RPC code
/// remains available.
#[derive(Debug, thiserror::Error)]
pub enum McpStreamableHttpError {
    #[error("invalid MCP Streamable HTTP configuration: {0}")]
    Config(#[from] McpStreamableHttpConfigError),

    #[error("invalid MCP endpoint: {0}")]
    Endpoint(#[from] EndpointError),

    #[error("failed to reach the MCP server: {0}")]
    Transport(String),

    #[error("MCP server answered the {method} request with HTTP {status}")]
    HttpStatus {
        method: &'static str,
        status: reqwest::StatusCode,
    },

    #[error(
        "MCP server answered with a redirect, which is not followed because it would send \
         credentials to an unvalidated origin"
    )]
    RedirectRefused,

    #[error(
        "MCP server answered with content type '{content_type}', expected application/json or \
         text/event-stream"
    )]
    UnexpectedContentType { content_type: String },

    #[error("MCP Streamable HTTP stream framing error: {0}")]
    Wire(#[from] SseWireError),

    #[error("MCP server's reply exceeded the {limit} byte limit")]
    ReplyTooLarge { limit: usize },

    #[error("MCP server kept paginating tools/list past {limit} pages")]
    TooManyToolPages { limit: usize },

    #[error("MCP server advertised more than {limit} tools")]
    TooManyTools { limit: usize },

    /// Rendered without the server's value, which is server-controlled text.
    #[error(
        "MCP server negotiated a protocol revision this client does not implement; \
         it supports {supported}"
    )]
    UnsupportedProtocolVersion { supported: String },

    #[error("MCP server returned JSON-RPC error: {0}")]
    JsonRpc(JsonRpcError),

    #[error("failed to parse the MCP response: {0}")]
    ParseError(String),

    /// The reply carried a different JSON-RPC id than the request.
    ///
    /// Kept distinct from a parse failure because it is not a malformed
    /// message: it is a well-formed answer to a different question, which a
    /// client that reads a reply off the wire and trusts its position rather
    /// than its id would accept.
    #[error("MCP server answered the {method} request with a reply for a different request id")]
    MismatchedReplyId { method: &'static str },

    /// The session the `initialize` handshake established is gone.
    ///
    /// Reported separately from a plain `404` because the remedy is different:
    /// the endpoint is fine and the connection should be re-established, not
    /// reconfigured.
    #[error("the MCP server no longer recognizes this session; it must be established again")]
    SessionExpired,

    #[error("timed out after {0:?} waiting for the MCP server")]
    Timeout(Duration),

    #[error("the MCP server closed the reply stream before answering the {method} request")]
    StreamClosed { method: &'static str },

    /// The request may have reached the server, but no reply arrived.
    ///
    /// The call may have executed. A `POST` that fails in transit, times out, or
    /// is merely accepted for processing proves nothing about whether the server
    /// ran the tool first, and this client cannot tell that apart from the tool
    /// never starting. Callers must not retry automatically: an MCP tool can
    /// send mail, charge a card, or write a file.
    #[error(
        "the MCP server may have received the '{method}' request but never answered it; \
         the call may have executed and must not be retried automatically"
    )]
    RequestIndeterminate { method: String },
}

/// The session state the transport itself owns.
///
/// Both values are assigned by the server during `initialize` and then echoed
/// on every later request. They are replaced as a unit rather than mutated
/// field by field, so a half-updated session cannot be observed.
#[derive(Debug, Clone, Default)]
struct Session {
    /// The `Mcp-Session-Id` the server assigned, when it assigned one.
    id: Option<HeaderValue>,
    /// The negotiated protocol revision, sent after the handshake.
    protocol_version: Option<HeaderValue>,
}

/// Which framing a reply arrived in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplyFraming {
    /// One `application/json` body holding the whole reply.
    Json,
    /// A `text/event-stream` carrying the reply as an SSE `message` event.
    EventStream,
}

/// A connected MCP server speaking the Streamable HTTP transport.
///
/// This is the low-level client. It performs the handshake, exposes the
/// server's advertised tools, and calls one selected tool. It deliberately does
/// not register anything with the runtime, so a host can apply its own
/// allowlists, redaction, and evidence policy over the top. Use
/// `RuntimeBuilder::with_mcp_streamable_http_server` when the generic bridging
/// behavior is what you want.
pub struct McpStreamableHttpClient {
    http: reqwest::Client,
    /// The one operator-configured endpoint every message is posted to.
    endpoint: Url,
    headers: HeaderMap,
    limits: McpStreamableHttpLimits,
    next_id: AtomicU64,
    session: Mutex<Session>,
    server_info: Option<McpServerInfo>,
    tools: Vec<McpToolDefinition>,
    server_name: String,
}

impl McpStreamableHttpClient {
    /// Performs the MCP handshake against the configured endpoint and
    /// discovers its tools.
    pub async fn connect(
        config: &McpStreamableHttpServerConfig,
    ) -> Result<Self, McpStreamableHttpError> {
        let endpoint = config.validate()?;
        let headers = build_headers(config)?;
        let http = build_http_client(&config.limits)?;

        let mut client = Self {
            http,
            endpoint,
            headers,
            limits: config.limits.clone(),
            next_id: AtomicU64::new(1),
            session: Mutex::new(Session::default()),
            server_info: None,
            tools: Vec::new(),
            server_name: config.name.clone(),
        };

        // A failure after `initialize` has to hand the session back. The
        // server has already allocated one by then, and dropping the client
        // sends no DELETE, so the session would sit on the server until it
        // timed out on its own.
        if let Err(error) = client.initialize().await {
            client.shutdown().await;
            return Err(error);
        }
        if let Err(error) = client.discover_tools().await {
            client.shutdown().await;
            return Err(error);
        }

        Ok(client)
    }

    /// The configured name of this server, used to namespace its tools.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// The configured MCP endpoint.
    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// Server information returned by the `initialize` handshake.
    pub fn server_info(&self) -> Option<&McpServerInfo> {
        self.server_info.as_ref()
    }

    /// The tools this server advertised.
    pub fn tools(&self) -> &[McpToolDefinition] {
        &self.tools
    }

    /// The session id the server assigned, if it assigned one.
    ///
    /// A server is free not to use sessions at all, in which case this stays
    /// `None` and no session header is ever sent.
    pub fn session_id(&self) -> Option<String> {
        lock_session(&self.session)
            .id
            .as_ref()
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    }

    /// Calls one tool on this server.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Option<JsonValue>,
    ) -> Result<McpToolCallResult, McpStreamableHttpError> {
        let params = McpToolCallParams {
            name: tool_name.to_string(),
            arguments,
        };
        self.request("tools/call", Some(params), self.limits.call_tool_timeout)
            .await
    }

    /// Ends the session, if the server established one.
    ///
    /// The specification lets a server refuse termination, so the outcome is
    /// deliberately ignored: there is nothing a caller could do about a server
    /// that declines to forget a session, and a shutdown path that can fail is
    /// a shutdown path that gets skipped.
    pub async fn shutdown(&self) {
        if lock_session(&self.session).id.is_none() {
            return;
        }

        let _ = tokio::time::timeout(
            self.limits.connect_timeout,
            self.http
                .delete(self.endpoint.clone())
                .headers(self.request_headers())
                .send(),
        )
        .await;

        // Forget the id whatever the server said. Keeping it would let a
        // second shutdown re-DELETE a session that is already gone, and would
        // let a later call present credentials for one that no longer exists.
        let mut session = lock_session(&self.session);
        *session = Session {
            id: None,
            protocol_version: session.protocol_version.clone(),
        };
    }

    /// Sends a JSON-RPC request, restarting the session if the server forgot it.
    async fn request<P: serde::Serialize, R: DeserializeOwned>(
        &self,
        method: &'static str,
        params: Option<P>,
        timeout: Duration,
    ) -> Result<R, McpStreamableHttpError> {
        let params = params
            .map(serde_json::to_value)
            .transpose()
            .map_err(|error| McpStreamableHttpError::ParseError(error.to_string()))?
            .filter(|params| !params.is_null());

        // A server may forget a session at any time, and the specification
        // requires the client to start a new one rather than give up. Without
        // this, one expired session left every tool from that server broken
        // for the runtime's whole life.
        //
        // Re-sending afterwards is safe even for a `tools/call`: a 404 for an
        // unknown session is refused before the message is dispatched, so the
        // tool did not run. The replacement carries a fresh id, because it is
        // a request in a new session rather than a repeat within the old one.
        let result = match self.attempt(method, params.clone(), timeout).await {
            Err(McpStreamableHttpError::SessionExpired) => {
                self.reestablish_session().await?;
                self.attempt(method, params, timeout).await?
            }
            other => other?,
        };

        serde_json::from_value(result).map_err(|_| {
            McpStreamableHttpError::ParseError("response shape did not match MCP".to_string())
        })
    }

    /// One attempt at a request, with no session recovery.
    ///
    /// The recovery path in [`request`](Self::request) runs its replacement
    /// handshake through this rather than through `request`, so a server that
    /// answers the recovery itself with a session error cannot start the
    /// recovery again. The impossibility is structural rather than a guard on
    /// the method name.
    async fn attempt(
        &self,
        method: &'static str,
        params: Option<JsonValue>,
        timeout: Duration,
    ) -> Result<JsonValue, McpStreamableHttpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = JsonRpcRequest::new(id, method, params);

        // One deadline covers this whole attempt, so a peer cannot evade
        // `call_tool_timeout` by accepting the connection and withholding
        // either the response head or the stream it promised.
        match tokio::time::timeout(timeout, self.exchange(method, &request, id)).await {
            Ok(result) => result,
            Err(_) => Err(request_timeout(method, timeout)),
        }
    }

    /// Posts one request and returns the JSON-RPC result it answered with.
    async fn exchange(
        &self,
        method: &'static str,
        request: &JsonRpcRequest,
        id: u64,
    ) -> Result<JsonValue, McpStreamableHttpError> {
        let response = self
            .post(method, request)
            .await
            .map_err(|error| classify_failure(method, error))?;

        // The session is assigned during the handshake and only then. Reading
        // it from a later response would let a server rotate the session
        // mid-conversation, silently moving in-flight work to a new one.
        if method == "initialize" {
            self.adopt_session(response.headers());
        }

        // Past this point the server has answered 2xx: the request reached the
        // application and a `tools/call` may already have run. Everything that
        // can go wrong between here and a parsed reply therefore leaves the
        // caller unable to tell whether the tool ran, so it is classified once,
        // here, rather than at each of the seven places it can fail.
        self.read_reply(method, response, id)
            .await
            .map_err(|error| reply_failure(method, error))
    }

    /// Reads the reply the server put on this response.
    async fn read_reply(
        &self,
        method: &'static str,
        response: reqwest::Response,
        id: u64,
    ) -> Result<JsonValue, McpStreamableHttpError> {
        // A 202 means accepted-for-processing with no reply, which is correct
        // for a notification and a protocol violation in answer to a request.
        if response.status() == reqwest::StatusCode::ACCEPTED {
            drain_bounded(response).await;
            return Err(McpStreamableHttpError::StreamClosed { method });
        }

        match reply_framing(&response)? {
            ReplyFraming::Json => {
                let body = self.read_bounded_body(response).await?;
                let reply = serde_json::from_slice::<JsonRpcResponse>(&body).map_err(|_| {
                    McpStreamableHttpError::ParseError("reply was not JSON-RPC".to_string())
                })?;
                outcome(method, reply, id)
            }
            ReplyFraming::EventStream => self.read_streamed_reply(method, response, id).await,
        }
    }

    /// Sends a JSON-RPC notification, which expects no reply.
    async fn notify(&self, method: &str, timeout: Duration) -> Result<(), McpStreamableHttpError> {
        let notification = serde_json::json!({"jsonrpc": "2.0", "method": method});

        // The deadline must cover the drain, not just the POST. `send()`
        // resolves on the response *head* and the body streams lazily, so a
        // server that promises a body with `Content-Length` and then goes
        // quiet leaves an undeadlined drain waiting forever -- and this
        // notification runs inside `connect`, which runs inside a serial loop
        // in `RuntimeBuilder::build_async`. One such server would hang the
        // whole runtime build rather than degrade its own connection.
        let operation = async {
            let response = self.post("POST", &notification).await?;
            // A notification's 202 carries no body, but draining keeps the
            // connection reusable for the rest of the handshake.
            drain_bounded(response).await;
            Ok(())
        };

        tokio::time::timeout(timeout, operation)
            .await
            .map_err(|_| McpStreamableHttpError::Timeout(timeout))?
    }

    /// `POST`s one JSON-RPC message and returns the accepted response.
    async fn post<T: serde::Serialize>(
        &self,
        method: &'static str,
        message: &T,
    ) -> Result<reqwest::Response, McpStreamableHttpError> {
        let response = self
            .http
            .post(self.endpoint.clone())
            .headers(self.request_headers())
            .header(reqwest::header::ACCEPT, ACCEPT_BOTH)
            .json(message)
            .send()
            .await
            .map_err(transport_error)?;

        let status = response.status();
        if status.is_redirection() {
            return Err(McpStreamableHttpError::RedirectRefused);
        }
        if !status.is_success() {
            let had_session = lock_session(&self.session).id.is_some();
            // Read a bounded prefix so the connection returns to the pool, then
            // discard it: the body is attacker-controlled and never surfaces.
            drain_bounded(response).await;

            // A 404 against an established session is how the specification
            // says a session has been forgotten.
            if status == reqwest::StatusCode::NOT_FOUND && had_session {
                return Err(McpStreamableHttpError::SessionExpired);
            }
            return Err(McpStreamableHttpError::HttpStatus { method, status });
        }

        Ok(response)
    }

    /// The headers every request carries: the configured ones plus session state.
    fn request_headers(&self) -> HeaderMap {
        let mut headers = self.headers.clone();
        let session = lock_session(&self.session).clone();

        if let Some(id) = session.id {
            headers.insert(HeaderName::from_static(SESSION_HEADER), id);
        }
        if let Some(version) = session.protocol_version {
            headers.insert(HeaderName::from_static(PROTOCOL_HEADER), version);
        }

        headers
    }

    /// Records the session id from the `initialize` response, if it is usable.
    ///
    /// The value is server-controlled and is echoed on every later request, so
    /// it is length-bounded and required to be a valid header value. An
    /// unusable id is ignored rather than fatal: a server that sends one is
    /// misbehaving, but the session is optional in the first place, so the
    /// conversation can continue without it.
    fn adopt_session(&self, headers: &reqwest::header::HeaderMap) {
        let id = headers
            .get(SESSION_HEADER)
            .filter(|value| !value.is_empty() && value.len() <= MAX_SESSION_ID_BYTES)
            // The specification restricts a session id to visible ASCII. The
            // parser upstream already rejects CR and LF, so this is not what
            // stands between us and request splitting -- it stops a value that
            // an intermediary disagreeing about obs-text could read
            // differently from us.
            .filter(|value| {
                value
                    .as_bytes()
                    .iter()
                    .all(|byte| (0x21..=0x7e).contains(byte))
            })
            .map(|value| {
                // The id authenticates the session on every later request, so
                // it is as sensitive as the token beside it: redacted in
                // `Debug` output and never HPACK-indexed.
                let mut value = value.clone();
                value.set_sensitive(true);
                value
            });

        let mut session = lock_session(&self.session);
        *session = Session {
            id,
            protocol_version: session.protocol_version.clone(),
        };
    }

    /// Records the negotiated protocol revision reported by `initialize`.
    ///
    /// The reported revision has to match one this client actually implements,
    /// or there is nothing honest to do with it: sending it back on every later
    /// request would claim a protocol this code does not speak, and substituting
    /// our own would claim agreement the server never gave.
    ///
    /// Because the match is against `&'static` constants, the value that ends up
    /// in the header is one of ours rather than the server's text. No
    /// server-controlled bytes reach a request header on this path at all.
    fn adopt_protocol_version(&self, reported: &str) -> Result<(), McpStreamableHttpError> {
        let negotiated = SUPPORTED_PROTOCOL_VERSIONS
            .into_iter()
            .find(|supported| *supported == reported)
            .ok_or_else(|| McpStreamableHttpError::UnsupportedProtocolVersion {
                supported: SUPPORTED_PROTOCOL_VERSIONS.join(", "),
            })?;

        let mut session = lock_session(&self.session);
        *session = Session {
            id: session.id.clone(),
            protocol_version: Some(HeaderValue::from_static(negotiated)),
        };
        Ok(())
    }

    /// Reads a whole `application/json` reply, bounded by the configured limit.
    async fn read_bounded_body(
        &self,
        response: reqwest::Response,
    ) -> Result<Vec<u8>, McpStreamableHttpError> {
        let limit = self.limits.max_response_bytes;
        let mut body = response.bytes_stream();
        let mut buffer = Vec::new();

        loop {
            let next = tokio::time::timeout(self.limits.stream_idle_timeout, body.next()).await;
            let chunk = match next {
                Ok(Some(Ok(chunk))) => chunk,
                Ok(Some(Err(_))) => {
                    return Err(McpStreamableHttpError::Transport(
                        "the reply body could not be read".to_string(),
                    ));
                }
                Ok(None) => break,
                Err(_) => {
                    return Err(McpStreamableHttpError::Timeout(
                        self.limits.stream_idle_timeout,
                    ));
                }
            };

            if buffer.len().saturating_add(chunk.len()) > limit {
                return Err(McpStreamableHttpError::ReplyTooLarge { limit });
            }
            buffer.extend_from_slice(&chunk);
        }

        Ok(buffer)
    }

    /// Reads an SSE-framed reply, returning the first event that answers `id`.
    ///
    /// A server may hold this stream open while it works, sending progress
    /// notifications and server-initiated requests. Those are not replies, so
    /// they are skipped rather than returned — the reply is identified by its
    /// JSON-RPC id, never by being the first thing on the stream.
    async fn read_streamed_reply(
        &self,
        method: &'static str,
        response: reqwest::Response,
        id: u64,
    ) -> Result<JsonValue, McpStreamableHttpError> {
        let mut parser = SseParser::new(self.limits.max_event_bytes);
        let mut body = response.bytes_stream();
        let mut seen = 0_usize;

        loop {
            let next = tokio::time::timeout(self.limits.stream_idle_timeout, body.next()).await;
            let chunk = match next {
                Ok(Some(Ok(chunk))) => chunk,
                // Any stream error is terminal, and the request may have run.
                Ok(Some(Err(_))) => return Err(indeterminate(method)),
                Ok(None) => break,
                Err(_) => return Err(request_timeout(method, self.limits.stream_idle_timeout)),
            };

            seen = seen.saturating_add(chunk.len());
            if seen > self.limits.max_response_bytes {
                // Individually legal events that never stop arriving are still
                // unbounded memory for the caller waiting on this reply.
                return Err(McpStreamableHttpError::ReplyTooLarge {
                    limit: self.limits.max_response_bytes,
                });
            }

            for event in parser.feed(&chunk)? {
                // Unknown event names are ignored rather than fatal; servers
                // and proxies both emit heartbeats the protocol never named.
                if event.event != "message" {
                    continue;
                }
                let Ok(reply) = serde_json::from_str::<JsonRpcResponse>(&event.data) else {
                    continue;
                };
                // A message with neither result nor error is a server-initiated
                // request, not a reply to anything.
                if reply.result.is_none() && reply.error.is_none() {
                    continue;
                }
                if reply.id != JsonRpcId::Number(id) {
                    continue;
                }
                return outcome(method, reply, id);
            }
        }

        // The stream ended without the reply. `reply_failure` decides what
        // that means for the method that asked.
        Err(McpStreamableHttpError::StreamClosed { method })
    }

    /// Performs the `initialize` handshake and the follow-up notification.
    async fn initialize(&mut self) -> Result<(), McpStreamableHttpError> {
        let result = self.handshake().await?;
        self.server_info = Some(result.server_info);
        Ok(())
    }

    /// Runs the handshake exchange and reports what the server said.
    ///
    /// Takes `&self` because it runs again mid-conversation when a server
    /// forgets the session, at which point nothing is holding a mutable borrow.
    async fn handshake(&self) -> Result<McpInitializeResult, McpStreamableHttpError> {
        let params = McpInitializeParams {
            protocol_version: PROTOCOL_VERSION.to_string(),
            capabilities: serde_json::json!({}),
            client_info: McpClientInfo {
                name: "mentra".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        };

        let params = serde_json::to_value(params)
            .map_err(|error| McpStreamableHttpError::ParseError(error.to_string()))?;
        let raw = self
            .attempt("initialize", Some(params), self.limits.initialize_timeout)
            .await?;
        let result: McpInitializeResult = serde_json::from_value(raw).map_err(|_| {
            McpStreamableHttpError::ParseError("response shape did not match MCP".to_string())
        })?;

        self.adopt_protocol_version(&result.protocol_version)?;
        self.notify("notifications/initialized", self.limits.initialize_timeout)
            .await?;

        Ok(result)
    }

    /// Starts a new session after the server said it no longer knows the old one.
    ///
    /// The specification requires the replacement `initialize` to carry no
    /// session at all, so the forgotten id is dropped first rather than sent to
    /// a server that has already rejected it.
    async fn reestablish_session(&self) -> Result<(), McpStreamableHttpError> {
        {
            let mut session = lock_session(&self.session);
            *session = Session::default();
        }
        self.handshake().await.map(|_| ())
    }

    /// Walks the paginated `tools/list` cursor to the end.
    async fn discover_tools(&mut self) -> Result<(), McpStreamableHttpError> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0_usize;

        loop {
            let params = McpListToolsParams {
                cursor: cursor.clone(),
            };
            let page: McpListToolsResult = self
                .request("tools/list", Some(params), self.limits.list_tools_timeout)
                .await?;
            tools.extend(page.tools);

            pages += 1;
            if tools.len() > self.limits.max_tools {
                // The page count and each page's size were bounded; the total
                // was not. A server willing to fill every page could push
                // gigabytes through this loop and be killed by the allocator
                // rather than by a limit.
                return Err(McpStreamableHttpError::TooManyTools {
                    limit: self.limits.max_tools,
                });
            }

            match page.next_cursor {
                // A missing or empty cursor means the last page.
                Some(next) if !next.is_empty() => {
                    // Checked only once another page is actually asked for, so
                    // a list exactly `max_tool_pages` long is accepted rather
                    // than refused for having reached the limit it is allowed
                    // to reach. A server that keeps handing back a cursor would
                    // otherwise loop forever; the cursor is opaque, so a repeat
                    // cannot be detected by value.
                    if pages >= self.limits.max_tool_pages {
                        return Err(McpStreamableHttpError::TooManyToolPages {
                            limit: self.limits.max_tool_pages,
                        });
                    }
                    cursor = Some(next);
                }
                _ => break,
            }
        }

        self.tools = tools;
        Ok(())
    }
}

impl std::fmt::Debug for McpStreamableHttpClient {
    /// Renders without the header map, which holds credentials.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpStreamableHttpClient")
            .field("server_name", &self.server_name)
            .field("endpoint", &self.endpoint.as_str())
            .field("tools", &self.tools.len())
            .finish_non_exhaustive()
    }
}

/// Turns one JSON-RPC reply into the result or error it carries.
fn outcome(
    method: &'static str,
    reply: JsonRpcResponse,
    id: u64,
) -> Result<JsonValue, McpStreamableHttpError> {
    if reply.id != JsonRpcId::Number(id) {
        return Err(McpStreamableHttpError::MismatchedReplyId { method });
    }

    match reply.error {
        Some(error) => Err(McpStreamableHttpError::JsonRpc(JsonRpcError {
            code: error.code,
            // The server's message is deliberately dropped; see the note on
            // `McpStreamableHttpError`.
            message: "server message omitted".to_string(),
            data: None,
        })),
        None => Ok(reply.result.unwrap_or(JsonValue::Null)),
    }
}

/// Decides which framing a successful response carries.
fn reply_framing(response: &reqwest::Response) -> Result<ReplyFraming, McpStreamableHttpError> {
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .trim_start()
        .to_ascii_lowercase();

    // Prefix match: both types commonly arrive with a charset parameter.
    if content_type.starts_with("application/json") {
        return Ok(ReplyFraming::Json);
    }
    if content_type.starts_with("text/event-stream") {
        return Ok(ReplyFraming::EventStream);
    }

    Err(McpStreamableHttpError::UnexpectedContentType {
        content_type: "[server value omitted]".to_string(),
    })
}

/// Reports a sent-but-unanswered request in the terms a caller needs.
///
/// Only `tools/call` is indeterminate: the handshake methods are idempotent, so
/// an unanswered one is simply a failed connection attempt.
fn indeterminate(method: &str) -> McpStreamableHttpError {
    if method == "tools/call" {
        McpStreamableHttpError::RequestIndeterminate {
            method: method.to_string(),
        }
    } else {
        McpStreamableHttpError::StreamClosed {
            method: "the handshake",
        }
    }
}

/// Converts a post-send failure into the certainty a caller needs.
///
/// Once the server has answered 2xx the request reached the application, so a
/// `tools/call` may already have run: a body that never arrives, a reply in a
/// framing this client cannot read, a stream that dies mid-flight and a reply
/// that does not parse are all indistinguishable from "the tool ran and the
/// answer was lost".
///
/// Two exceptions. A JSON-RPC error *is* the answer -- the tool ran and
/// reported failure -- and an already-indeterminate error is left alone rather
/// than re-wrapped.
fn reply_failure(method: &str, error: McpStreamableHttpError) -> McpStreamableHttpError {
    if method != "tools/call" {
        return error;
    }

    match error {
        McpStreamableHttpError::JsonRpc(_)
        | McpStreamableHttpError::RequestIndeterminate { .. } => error,
        _ => indeterminate(method),
    }
}

/// Converts a request failure into the method-level certainty a caller needs.
///
/// Once a `tools/call` request has been sent, neither a transport error nor a
/// server error status proves the application did not process its body first.
/// A 4xx is the exception: those are transport-level rejections the server
/// makes before dispatching the message — an unacceptable `Accept` header, a
/// missing credential, a forgotten session — so they are reported as
/// themselves. A 5xx is not, because a server can fail after running the tool.
fn classify_failure(method: &str, error: McpStreamableHttpError) -> McpStreamableHttpError {
    if method != "tools/call" {
        return error;
    }

    match &error {
        McpStreamableHttpError::HttpStatus { status, .. } if status.is_client_error() => error,
        // A redirect is deliberately *not* here. RFC 9110 15.4.4 makes a 303
        // answering a POST the server saying it processed the request and put
        // the result elsewhere; refusing to follow it is right, calling it
        // "definitely did not run" is not.
        McpStreamableHttpError::SessionExpired
        | McpStreamableHttpError::Config(_)
        | McpStreamableHttpError::Endpoint(_) => error,
        _ => indeterminate(method),
    }
}

/// Reports expiration of the whole request operation.
fn request_timeout(method: &str, timeout: Duration) -> McpStreamableHttpError {
    if method == "tools/call" {
        indeterminate(method)
    } else {
        McpStreamableHttpError::Timeout(timeout)
    }
}

/// Acquires the session even if another task panicked while holding it.
///
/// No critical section runs user code or performs I/O, so recovering the
/// contained value is safe, and losing the session on poison would strand a
/// client that is otherwise still usable.
fn lock_session(session: &Mutex<Session>) -> std::sync::MutexGuard<'_, Session> {
    session
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Builds the HTTP client used for every request to the endpoint.
fn build_http_client(
    limits: &McpStreamableHttpLimits,
) -> Result<reqwest::Client, McpStreamableHttpError> {
    reqwest::Client::builder()
        // Redirects are never followed. reqwest strips `Authorization` only
        // across hosts and compares host and port without scheme, so an
        // https->http redirect on the same host would keep the credential and
        // send it in the clear. The request body is never stripped at all, so a
        // followed redirect would also hand tool arguments to the new target.
        .redirect(reqwest::redirect::Policy::none())
        // Reqwest retries selected protocol-level rejections on its own once
        // the negotiated protocol can signal them (HTTP/2 REFUSED_STREAM and
        // kin). A tools/call may already have executed by the time such a
        // signal arrives, so an automatic resend would replay a side-effecting
        // call with no caller involvement.
        .retry(reqwest::retry::never())
        .connect_timeout(limits.connect_timeout)
        // Deliberately no `.timeout()`: a reply may arrive as a stream the
        // server holds open while it works, and a total deadline here would cut
        // it off on a fixed interval. Each request applies its own deadline.
        .build()
        .map_err(|error| McpStreamableHttpError::Transport(error.to_string()))
}

/// Converts configured headers into a map, marking every value sensitive.
fn build_headers(
    config: &McpStreamableHttpServerConfig,
) -> Result<HeaderMap, McpStreamableHttpError> {
    let mut headers = HeaderMap::new();

    for (name, value) in &config.headers {
        let name = HeaderName::try_from(name.as_str()).map_err(|_| {
            McpStreamableHttpConfigError::InvalidHeaderName {
                name: name.to_string(),
            }
        })?;
        let mut value = HeaderValue::try_from(value.expose_secret()).map_err(|_| {
            McpStreamableHttpConfigError::InvalidHeaderValue {
                name: name.to_string(),
            }
        })?;
        // A plain HeaderValue prints its contents in Debug output. Marking it
        // sensitive redacts it there and tells HTTP/2 not to index it.
        value.set_sensitive(true);
        headers.insert(name, value);
    }

    Ok(headers)
}

/// Reads and discards a bounded prefix of a response body.
async fn drain_bounded(response: reqwest::Response) {
    let mut body = response.bytes_stream();
    let mut seen = 0_usize;
    while let Some(Ok(chunk)) = body.next().await {
        seen += chunk.len();
        if seen >= MAX_DIAGNOSTIC_BODY_BYTES {
            break;
        }
    }
}

/// Renders a transport failure without echoing server-controlled text.
fn transport_error(error: reqwest::Error) -> McpStreamableHttpError {
    let reason = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "could not connect"
    } else if error.is_request() {
        "the request could not be sent"
    } else {
        "the connection failed"
    };
    McpStreamableHttpError::Transport(reason.to_string())
}
