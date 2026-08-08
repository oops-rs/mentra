//! MCP client for the legacy HTTP+SSE transport (protocol revision 2024-11-05).
//!
//! # The transport
//!
//! This is the *older* MCP HTTP transport, not Streamable HTTP. The two are
//! easy to confuse and are not interchangeable:
//!
//! | | legacy HTTP+SSE (this module) | Streamable HTTP |
//! |---|---|---|
//! | Endpoints | a `GET` stream plus a separate `POST` URL | one URL for both |
//! | POST target | named by the server in an `endpoint` event | the configured URL |
//! | Responses | always on the `GET` stream | in the POST response or a stream |
//! | Session | a query parameter in the endpoint URL | the `Mcp-Session-Id` header |
//!
//! Servers that answer `404` on `/mcp` but serve `/sse` require this transport.
//!
//! # Lifecycle
//!
//! 1. `GET` the configured URL with `Accept: text/event-stream`.
//! 2. Wait for an `event: endpoint` frame naming the `POST` URL, resolve it
//!    against the configured URL, and require it to stay on the same origin.
//! 3. `POST` JSON-RPC requests as `application/json`. Any 2xx — including the
//!    `202 Accepted` both reference servers return — means the message was
//!    accepted for processing, not that it completed.
//! 4. Read JSON-RPC responses from `event: message` frames on the stream and
//!    correlate them to requests by id.
//!
//! The handshake is `initialize`, then a `notifications/initialized`
//! notification, then a paginated `tools/list`.
//!
//! # Failure behavior
//!
//! The stream carries every response, so losing it ends the session. This
//! client fails closed: when the stream ends, every pending request resolves
//! with an error rather than hanging. It never reconnects and never re-sends a
//! `tools/call`, because an MCP tool may have side effects and a transparent
//! retry would execute it twice with no caller involvement.
//!
//! A `tools/call` whose `POST` was accepted but whose response never arrived is
//! reported as [`McpSseError::RequestIndeterminate`] rather than as a plain
//! failure, so a caller can tell "may have run" apart from "definitely did not".

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::de::DeserializeOwned;
use serde_json::Value as JsonValue;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use url::Url;

use super::config::{McpSseConfigError, McpSseLimits, McpSseServerConfig};
use super::endpoint::{EndpointError, resolve_endpoint};
use super::wire::{SseParser, SseWireError};
use crate::mcp::protocol::*;

/// The protocol revision this transport implements.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Bound on how much of an HTTP error body is read for diagnostics.
///
/// The body is attacker-controlled, so it is never included in an error; this
/// bound exists only so that reading and discarding it cannot be turned into a
/// memory-exhaustion primitive.
const MAX_DIAGNOSTIC_BODY_BYTES: usize = 8 * 1024;

/// Errors from the MCP SSE client.
///
/// No variant carries a response body, an SSE payload, a tool argument, or a
/// tool result. Server text is never interpolated into an error, because a
/// malicious server would otherwise be able to write arbitrary content —
/// including forged log lines and terminal escape sequences — into an
/// operator's logs.
#[derive(Debug, thiserror::Error)]
pub enum McpSseError {
    #[error("invalid MCP SSE configuration: {0}")]
    Config(#[from] McpSseConfigError),

    #[error("invalid MCP SSE endpoint: {0}")]
    Endpoint(#[from] EndpointError),

    #[error("failed to reach the MCP SSE server: {0}")]
    Transport(String),

    #[error("MCP SSE server answered the {method} request with HTTP {status}")]
    HttpStatus {
        method: &'static str,
        status: reqwest::StatusCode,
    },

    #[error(
        "MCP SSE server answered with a redirect, which is not followed because it would send \
         credentials to an unvalidated origin"
    )]
    RedirectRefused,

    #[error(
        "MCP SSE server answered with content type '{content_type}', expected text/event-stream"
    )]
    UnexpectedContentType { content_type: String },

    #[error("MCP SSE stream framing error: {0}")]
    Wire(#[from] SseWireError),

    #[error("MCP SSE endpoint event exceeded the {limit} byte limit")]
    EndpointTooLarge { limit: usize },

    #[error("MCP SSE server returned JSON-RPC error: {0}")]
    JsonRpc(JsonRpcError),

    #[error("failed to parse the MCP SSE response: {0}")]
    ParseError(String),

    #[error("timed out after {0:?} waiting for the MCP SSE server")]
    Timeout(Duration),

    #[error("the MCP SSE stream closed before the request completed")]
    StreamClosed,

    /// The request was sent but no response arrived before the stream ended or
    /// the deadline passed.
    ///
    /// The call may have executed. The `POST` and the response travel on
    /// different connections, so a server can accept and run a tool while the
    /// stream dies, and the client cannot tell that apart from the tool never
    /// starting. Callers must not retry automatically: an MCP tool can send
    /// mail, charge a card, or write a file.
    ///
    /// This is distinct from [`McpSseError::HttpStatus`] on a `POST`, which
    /// means the server rejected the message and it definitely did not run.
    #[error(
        "the MCP SSE server accepted the '{method}' request but never answered it; \
         the call may have executed and must not be retried automatically"
    )]
    RequestIndeterminate { method: String },

    #[error("the MCP SSE client is shut down")]
    Shutdown,
}

/// The outcome of a JSON-RPC request, kept in the pending map.
type PendingReply = Result<JsonValue, McpSseError>;

/// Correlates in-flight requests to the responses arriving on the stream.
///
/// Lookups remove the entry, so the first response for an id wins and a
/// malicious server cannot deliver a second result for a call the caller has
/// already observed.
#[derive(Default)]
struct Pending {
    waiters: HashMap<u64, PendingWaiter>,
    /// Set once the stream ends so late requests fail immediately rather than
    /// waiting out their timeout.
    closed: bool,
}

struct PendingWaiter {
    reply: oneshot::Sender<PendingReply>,
    method: String,
}

/// A connected MCP server speaking the legacy HTTP+SSE transport.
///
/// This is the low-level client. It performs the handshake, exposes the
/// server's advertised tools, and calls one selected tool. It deliberately does
/// not register anything with the runtime, so a host can apply its own
/// allowlists, redaction, and evidence policy over the top. Use
/// [`RuntimeBuilder::with_mcp_sse_server`](crate::runtime::RuntimeBuilder::with_mcp_sse_server)
/// when the generic bridging behavior is what you want.
pub struct McpSseClient {
    http: reqwest::Client,
    /// The `POST` target named by the server, validated to the configured origin.
    endpoint: Url,
    headers: HeaderMap,
    limits: McpSseLimits,
    next_id: AtomicU64,
    pending: Arc<Mutex<Pending>>,
    reader: JoinHandle<()>,
    server_info: Option<McpServerInfo>,
    tools: Vec<McpToolDefinition>,
    server_name: String,
    stream_url: Url,
}

impl McpSseClient {
    /// Opens the SSE stream, performs the MCP handshake, and discovers tools.
    pub async fn connect(config: &McpSseServerConfig) -> Result<Self, McpSseError> {
        let stream_url = config.validate()?;
        let headers = build_headers(config)?;
        let http = build_http_client(&config.limits)?;

        let response = tokio::time::timeout(
            config.limits.connect_timeout,
            http.get(stream_url.clone())
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .headers(headers.clone())
                .send(),
        )
        .await
        .map_err(|_| McpSseError::Timeout(config.limits.connect_timeout))?
        .map_err(transport_error)?;

        check_stream_response(&response)?;

        let pending: Arc<Mutex<Pending>> = Arc::new(Mutex::new(Pending::default()));
        let (endpoint_tx, endpoint_rx) = oneshot::channel();

        let reader = tokio::spawn(read_stream(
            response,
            Arc::clone(&pending),
            endpoint_tx,
            config.limits.clone(),
        ));

        // The endpoint event must arrive before anything can be sent. Bound the
        // wait: a buffering proxy is a common cause of it never arriving.
        let endpoint = match tokio::time::timeout(config.limits.connect_timeout, endpoint_rx).await
        {
            Ok(Ok(Ok(raw))) => resolve_endpoint(&stream_url, &raw)?,
            Ok(Ok(Err(error))) => {
                reader.abort();
                return Err(error);
            }
            Ok(Err(_)) => {
                reader.abort();
                return Err(McpSseError::StreamClosed);
            }
            Err(_) => {
                reader.abort();
                return Err(McpSseError::Timeout(config.limits.connect_timeout));
            }
        };

        let mut client = Self {
            http,
            endpoint,
            headers,
            limits: config.limits.clone(),
            next_id: AtomicU64::new(1),
            pending,
            reader,
            server_info: None,
            tools: Vec::new(),
            server_name: config.name.clone(),
            stream_url,
        };

        client.initialize().await?;
        client.discover_tools().await?;

        Ok(client)
    }

    /// The configured name of this server, used to namespace its tools.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// The configured SSE stream URL.
    pub fn stream_url(&self) -> &Url {
        &self.stream_url
    }

    /// Server information returned by the `initialize` handshake.
    pub fn server_info(&self) -> Option<&McpServerInfo> {
        self.server_info.as_ref()
    }

    /// The tools this server advertised.
    pub fn tools(&self) -> &[McpToolDefinition] {
        &self.tools
    }

    /// Calls one tool on this server.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Option<JsonValue>,
    ) -> Result<McpToolCallResult, McpSseError> {
        let params = McpToolCallParams {
            name: tool_name.to_string(),
            arguments,
        };
        self.request("tools/call", Some(params), self.limits.call_tool_timeout)
            .await
    }

    /// Closes the stream and fails every request still in flight.
    pub async fn shutdown(&self) {
        self.reader.abort();
        let mut pending = self.pending.lock().await;
        pending.closed = true;
        drain_pending(&mut pending);
    }

    /// Sends a JSON-RPC request and waits for its correlated response.
    async fn request<P: serde::Serialize, R: DeserializeOwned>(
        &self,
        method: &'static str,
        params: Option<P>,
        timeout: Duration,
    ) -> Result<R, McpSseError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let params = params
            .map(|params| serde_json::to_value(params))
            .transpose()
            .map_err(|error| McpSseError::ParseError(error.to_string()))?
            .filter(|params| !params.is_null());
        let request = JsonRpcRequest::new(id, method, params);

        let (reply_tx, reply_rx) = oneshot::channel();
        {
            // Register before sending. The server answers the POST before it
            // processes the message, so the response can reach the stream
            // before the POST future resolves.
            let mut pending = self.pending.lock().await;
            if pending.closed {
                return Err(McpSseError::StreamClosed);
            }
            pending.waiters.insert(
                id,
                PendingWaiter {
                    reply: reply_tx,
                    method: method.to_string(),
                },
            );
        }

        if let Err(error) = self.post(&request).await {
            // A failed POST is the one case where the request definitely did
            // not reach the server, so the registration is dropped rather than
            // left to time out — and the caller gets a definite failure.
            self.forget(id).await;
            return Err(error);
        }

        let result = match tokio::time::timeout(timeout, reply_rx).await {
            Ok(Ok(result)) => result?,
            // The reader dropped the sender, which only happens on teardown.
            Ok(Err(_)) => return Err(indeterminate(method)),
            Err(_) => {
                // Remove the registration so a timed-out request cannot leak an
                // entry for the lifetime of the connection.
                self.forget(id).await;
                return Err(if method == "tools/call" {
                    indeterminate(method)
                } else {
                    McpSseError::Timeout(timeout)
                });
            }
        };

        serde_json::from_value(result)
            .map_err(|error| McpSseError::ParseError(format!("deserialize response: {error}")))
    }

    /// Removes a registration whose request will never be answered.
    async fn forget(&self, id: u64) {
        self.pending.lock().await.waiters.remove(&id);
    }

    /// Sends a JSON-RPC notification, which expects no response.
    async fn notify(&self, method: &str) -> Result<(), McpSseError> {
        let notification = serde_json::json!({"jsonrpc": "2.0", "method": method});
        self.post(&notification).await
    }

    /// `POST`s one JSON-RPC message to the validated endpoint.
    async fn post<T: serde::Serialize>(&self, message: &T) -> Result<(), McpSseError> {
        let response = self
            .http
            .post(self.endpoint.clone())
            .headers(self.headers.clone())
            .json(message)
            .send()
            .await
            .map_err(transport_error)?;

        let status = response.status();
        if status.is_redirection() {
            return Err(McpSseError::RedirectRefused);
        }
        if !status.is_success() {
            // Read a bounded prefix so the connection returns to the pool, then
            // discard it: the body is attacker-controlled and never surfaces.
            drain_bounded(response).await;
            return Err(McpSseError::HttpStatus {
                method: "POST",
                status,
            });
        }

        // The JSON-RPC result never arrives here — it comes back on the stream.
        // Drain anyway so the connection is reusable.
        drain_bounded(response).await;
        Ok(())
    }

    /// Performs the `initialize` handshake and the follow-up notification.
    async fn initialize(&mut self) -> Result<(), McpSseError> {
        let params = McpInitializeParams {
            protocol_version: PROTOCOL_VERSION.to_string(),
            capabilities: serde_json::json!({}),
            client_info: McpClientInfo {
                name: "mentra".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        };

        let result: McpInitializeResult = self
            .request("initialize", Some(params), self.limits.initialize_timeout)
            .await?;
        self.server_info = Some(result.server_info);

        self.notify("notifications/initialized").await
    }

    /// Walks the paginated `tools/list` cursor to the end.
    async fn discover_tools(&mut self) -> Result<(), McpSseError> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;

        loop {
            let params = McpListToolsParams {
                cursor: cursor.clone(),
            };
            let page: McpListToolsResult = self
                .request("tools/list", Some(params), self.limits.list_tools_timeout)
                .await?;
            tools.extend(page.tools);

            match page.next_cursor {
                // A missing or empty cursor means the last page.
                Some(next) if !next.is_empty() => cursor = Some(next),
                _ => break,
            }
        }

        self.tools = tools;
        Ok(())
    }
}

impl Drop for McpSseClient {
    fn drop(&mut self) {
        // Cancel the reader so the task and its connection do not outlive the
        // client that owns them.
        self.reader.abort();
    }
}

impl std::fmt::Debug for McpSseClient {
    /// Renders without the header map, which holds credentials.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpSseClient")
            .field("server_name", &self.server_name)
            .field("stream_url", &self.stream_url.as_str())
            .field("tools", &self.tools.len())
            .finish_non_exhaustive()
    }
}

/// Reads the SSE stream until it ends, routing frames to waiting callers.
async fn read_stream(
    response: reqwest::Response,
    pending: Arc<Mutex<Pending>>,
    endpoint_tx: oneshot::Sender<Result<String, McpSseError>>,
    limits: McpSseLimits,
) {
    let mut parser = SseParser::new(limits.max_event_bytes);
    let mut body = response.bytes_stream();
    let mut endpoint_tx = Some(endpoint_tx);

    loop {
        let next = tokio::time::timeout(limits.stream_idle_timeout, body.next()).await;

        let chunk = match next {
            Ok(Some(Ok(chunk))) => chunk,
            // Any stream error is terminal. reqwest's `is_body` does not
            // reliably identify body errors, so it is not consulted.
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => break,
        };

        let events = match parser.feed(&chunk) {
            Ok(events) => events,
            Err(error) => {
                // Framing can no longer be trusted, so the stream is torn down
                // rather than resynchronized at an attacker-chosen boundary.
                notify_endpoint(&mut endpoint_tx, Err(McpSseError::Wire(error)));
                break;
            }
        };

        for event in events {
            match event.event.as_str() {
                "endpoint" => {
                    if event.data.len() > limits.max_endpoint_bytes {
                        notify_endpoint(
                            &mut endpoint_tx,
                            Err(McpSseError::EndpointTooLarge {
                                limit: limits.max_endpoint_bytes,
                            }),
                        );
                        // Fail closed: without a usable endpoint nothing can be sent.
                        break;
                    }
                    // Only the first endpoint event is honored. A later one
                    // would silently redirect in-flight traffic.
                    notify_endpoint(&mut endpoint_tx, Ok(event.data));
                }
                "message" => deliver_message(&pending, &event.data).await,
                // Unknown event names, including the `ping` frames older
                // sse-starlette versions emit, are ignored rather than fatal.
                _ => {}
            }
        }
    }

    // Whatever ended the stream, no further response can arrive.
    notify_endpoint(&mut endpoint_tx, Err(McpSseError::StreamClosed));
    let mut pending = pending.lock().await;
    pending.closed = true;
    drain_pending(&mut pending);
}

/// Routes one `message` frame to the caller waiting on its id.
async fn deliver_message(pending: &Arc<Mutex<Pending>>, data: &str) {
    let Ok(response) = serde_json::from_str::<JsonRpcResponse>(data) else {
        // Malformed JSON, or a server-initiated request such as `ping`. Neither
        // is a response, so neither is correlated. Dropping it keeps a hostile
        // server from turning unsolicited frames into per-id state.
        return;
    };

    // A response carries a result or an error; anything else is a request.
    if response.result.is_none() && response.error.is_none() {
        return;
    }

    let JsonRpcId::Number(id) = response.id else {
        return;
    };

    // Removing the entry means the first response wins and a repeated id
    // cannot deliver a second result for an already-observed call.
    let Some(waiter) = pending.lock().await.waiters.remove(&id) else {
        return;
    };

    let reply = match response.error {
        Some(error) => Err(McpSseError::JsonRpc(error)),
        None => Ok(response.result.unwrap_or(JsonValue::Null)),
    };
    let _ = waiter.reply.send(reply);
}

/// Sends the endpoint outcome exactly once.
fn notify_endpoint(
    endpoint_tx: &mut Option<oneshot::Sender<Result<String, McpSseError>>>,
    outcome: Result<String, McpSseError>,
) {
    if let Some(tx) = endpoint_tx.take() {
        let _ = tx.send(outcome);
    }
}

/// Fails every pending request.
///
/// A request that is still registered was sent — the only path that removes a
/// registration without a response is a failed `POST`. So a `tools/call` here
/// may have executed, and saying so is what lets a caller avoid re-running a
/// non-idempotent action.
fn drain_pending(pending: &mut Pending) {
    for (_, waiter) in pending.waiters.drain() {
        let _ = waiter.reply.send(Err(indeterminate(&waiter.method)));
    }
}

/// Reports a sent-but-unanswered request in the terms a caller needs.
///
/// Only `tools/call` is reported as indeterminate: the handshake methods are
/// idempotent, so an unanswered one is simply a closed stream.
fn indeterminate(method: &str) -> McpSseError {
    if method == "tools/call" {
        McpSseError::RequestIndeterminate {
            method: method.to_string(),
        }
    } else {
        McpSseError::StreamClosed
    }
}

/// Builds the HTTP client shared by the stream and the message endpoint.
fn build_http_client(limits: &McpSseLimits) -> Result<reqwest::Client, McpSseError> {
    reqwest::Client::builder()
        // Redirects are never followed. reqwest strips `Authorization` only
        // across hosts and compares host and port without scheme, so an
        // https->http redirect on the same host would keep the credential and
        // send it in the clear. The request body is never stripped at all, so a
        // followed redirect would also hand tool arguments to the new target.
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(limits.connect_timeout)
        // Deliberately no `.timeout()`: that is a total deadline covering the
        // response body, which would kill the long-lived stream on a fixed
        // interval. Request deadlines are enforced against the pending map.
        .build()
        .map_err(|error| McpSseError::Transport(error.to_string()))
}

/// Converts configured headers into a map, marking every value sensitive.
fn build_headers(config: &McpSseServerConfig) -> Result<HeaderMap, McpSseError> {
    let mut headers = HeaderMap::new();

    for (name, value) in &config.headers {
        let name = HeaderName::try_from(name.as_str()).map_err(|_| {
            McpSseConfigError::InvalidHeaderName {
                name: name.to_string(),
            }
        })?;
        let mut value = HeaderValue::try_from(value.expose_secret()).map_err(|_| {
            McpSseConfigError::InvalidHeaderValue {
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

/// Requires a 200 response carrying an event stream.
fn check_stream_response(response: &reqwest::Response) -> Result<(), McpSseError> {
    let status = response.status();
    if status.is_redirection() {
        return Err(McpSseError::RedirectRefused);
    }
    if !status.is_success() {
        return Err(McpSseError::HttpStatus {
            method: "GET",
            status,
        });
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();

    // Prefix match: servers commonly send `text/event-stream; charset=utf-8`.
    if !content_type
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("text/event-stream")
    {
        return Err(McpSseError::UnexpectedContentType {
            content_type: content_type.to_string(),
        });
    }

    Ok(())
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
fn transport_error(error: reqwest::Error) -> McpSseError {
    let reason = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "could not connect"
    } else if error.is_request() {
        "the request could not be sent"
    } else {
        "the connection failed"
    };
    McpSseError::Transport(reason.to_string())
}
