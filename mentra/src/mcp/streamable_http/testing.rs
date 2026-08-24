//! A local Streamable HTTP MCP server for tests.
//!
//! Tests run against a real [`TcpListener`] rather than a mock so that the
//! bytes on the wire are asserted: the single endpoint, the `Accept` header
//! that a streamable-only server rejects a request without, and the
//! `Mcp-Session-Id` round trip. A mock of `reqwest` would assert the client's
//! intent instead of what a server actually receives, and the `406` that
//! motivated this transport was exactly a header the client never sent.
//!
//! [`TcpListener`]: std::net::TcpListener

use std::collections::HashMap;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{Value, json};

use crate::mcp::testing::{CapturedRequest, read_request};

/// How the fixture delivers a JSON-RPC reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplyMode {
    /// One `application/json` body per reply.
    Json,
    /// A `text/event-stream` body carrying the reply as an SSE `message` event.
    ///
    /// The stream also carries a progress notification *before* the reply, so a
    /// client that returns the first event it sees rather than the one matching
    /// its request id fails the test.
    EventStream,
}

/// A running fixture server.
pub(crate) struct StreamableHttpFixture {
    endpoint: String,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
}

impl StreamableHttpFixture {
    /// Starts a server that answers the MCP handshake and one tool.
    pub(crate) fn start(mode: ReplyMode) -> Self {
        Self::start_with(mode, ServerBehavior::default())
    }

    /// Starts a server with non-default behavior.
    pub(crate) fn start_with(mode: ReplyMode, behavior: ServerBehavior) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture listener");
        let port = listener.local_addr().expect("fixture address").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);

        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                // Keep-alive: one connection may carry the whole handshake.
                while let Some(request) = read_request(&mut stream) {
                    captured
                        .lock()
                        .expect("fixture request log")
                        .push(request.clone());
                    if !answer(&mut stream, &request, mode, &behavior) {
                        break;
                    }
                }
            }
        });

        Self {
            endpoint: format!("http://127.0.0.1:{port}/mcp"),
            requests,
        }
    }

    /// The single MCP endpoint this server serves.
    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Every request the server has received, in order.
    pub(crate) fn requests(&self) -> Vec<CapturedRequest> {
        self.requests
            .lock()
            .expect("fixture request log")
            .iter()
            .cloned()
            .collect()
    }

    /// The requests carrying a JSON-RPC method, paired with that method.
    pub(crate) fn rpc_methods(&self) -> Vec<String> {
        self.requests()
            .iter()
            .filter_map(CapturedRequest::rpc_method)
            .collect()
    }
}

/// Deviations from a well-behaved server, one per test that needs one.
#[derive(Debug, Clone, Default)]
pub(crate) struct ServerBehavior {
    /// The session id handed out during `initialize`, if any.
    pub(crate) session_id: Option<String>,
    /// Answer `tools/call` with a JSON-RPC error instead of a result.
    pub(crate) tool_call_fails: bool,
    /// Answer this JSON-RPC method with an id that is not the one requested.
    pub(crate) mismatch_reply_id_on: Option<String>,
    /// Answer every request with this HTTP status and an empty body.
    pub(crate) http_status: Option<u16>,
    /// The protocol version reported by `initialize`.
    pub(crate) protocol_version: Option<String>,
    /// Write the response head for this JSON-RPC method, then stop.
    ///
    /// The head promises a body with `Content-Length` and none arrives, and the
    /// connection stays open. This is the shape a client must survive and the
    /// one no fixture here could produce: `reqwest` resolves `send()` on the
    /// head, so any read of the body afterwards that is not itself bounded
    /// waits forever.
    pub(crate) stall_after_headers_on: Option<String>,
    /// Serve `tools/list` as this many pages, chained by `nextCursor`.
    ///
    /// `None` serves one page with no cursor. The walk was previously untested
    /// in either direction, which is how an off-by-one in its page cap
    /// survived.
    pub(crate) tool_pages: Option<usize>,
    /// Always hand back a `nextCursor`, so the walk only ends at a limit.
    pub(crate) paginates_forever: bool,
    /// Answer `tools/call` with this HTTP status, after a successful handshake.
    ///
    /// Distinct from `http_status`, which applies to every request and so
    /// cannot get past `initialize` -- which is why nothing could reach the
    /// post-handshake failure classification.
    pub(crate) tool_call_status: Option<u16>,
}

impl ServerBehavior {
    /// A server that assigns a session id.
    pub(crate) fn with_session(session_id: impl Into<String>) -> Self {
        Self {
            session_id: Some(session_id.into()),
            ..Self::default()
        }
    }
}

/// Writes one response. Returns whether the connection stays usable.
fn answer(
    stream: &mut TcpStream,
    request: &CapturedRequest,
    mode: ReplyMode,
    behavior: &ServerBehavior,
) -> bool {
    if let Some(status) = behavior.http_status {
        return write_raw(stream, status, "text/plain", "", &HashMap::new());
    }

    if let Some(stall_on) = &behavior.stall_after_headers_on
        && request.rpc_method().as_deref() == Some(stall_on.as_str())
    {
        // A head promising a body, and then silence. The connection is left
        // open deliberately: closing it would give the client an EOF, which is
        // the case that already worked.
        let framing = if request.rpc_id().is_some() {
            "application/json"
        } else {
            "text/plain"
        };
        let head =
            format!("HTTP/1.1 200 OK\r\nContent-Type: {framing}\r\nContent-Length: 4096\r\n\r\n");
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.flush();
        return true;
    }

    // Session termination, which the client sends on shutdown.
    if request.method == "DELETE" {
        return write_raw(stream, 200, "text/plain", "", &HashMap::new());
    }

    let Some(method) = request.rpc_method() else {
        return write_raw(stream, 400, "text/plain", "", &HashMap::new());
    };

    // A notification has no id and expects no reply body.
    let Some(id) = request.rpc_id() else {
        return write_raw(stream, 202, "text/plain", "", &HashMap::new());
    };

    if let Some(status) = behavior.tool_call_status
        && method == "tools/call"
    {
        return write_raw(stream, status, "text/plain", "", &HashMap::new());
    }

    let result = match method.as_str() {
        "initialize" => json!({
            "protocolVersion": behavior
                .protocol_version
                .clone()
                .unwrap_or_else(|| "2025-06-18".to_string()),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fixture", "version": "9.9.9"},
        }),
        "tools/list" => {
            // The cursor carries the page index, so the fixture stays
            // stateless across the connection.
            let served = request
                .rpc_cursor()
                .and_then(|cursor| cursor.strip_prefix("page-").map(str::to_string))
                .and_then(|index| index.parse::<usize>().ok())
                .unwrap_or(0);
            let total = behavior.tool_pages.unwrap_or(1);
            let more = behavior.paginates_forever || served + 1 < total;

            let mut page = json!({
                "tools": [{
                    "name": format!("echo{}", if served == 0 { String::new() } else { served.to_string() }),
                    "description": "Echoes its argument",
                    "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}},
                }],
            });
            if more {
                page["nextCursor"] = json!(format!("page-{}", served + 1));
            }
            page
        }
        "tools/call" => {
            if behavior.tool_call_fails {
                return write_reply(
                    stream,
                    mode,
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "tool exploded"}}),
                    session_headers(&method, behavior),
                );
            }
            json!({"content": [{"type": "text", "text": "echoed"}], "isError": false})
        }
        _ => {
            return write_raw(stream, 404, "text/plain", "", &HashMap::new());
        }
    };

    let reply_id = if behavior.mismatch_reply_id_on.as_deref() == Some(method.as_str()) {
        json!(id + 1_000)
    } else {
        json!(id)
    };

    write_reply(
        stream,
        mode,
        json!({"jsonrpc": "2.0", "id": reply_id, "result": result}),
        session_headers(&method, behavior),
    )
}

/// The extra headers a reply carries, which is the session id at `initialize`.
fn session_headers(method: &str, behavior: &ServerBehavior) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    if method == "initialize"
        && let Some(session_id) = &behavior.session_id
    {
        headers.insert("Mcp-Session-Id".to_string(), session_id.clone());
    }
    headers
}

/// Writes a JSON-RPC reply in whichever framing the fixture was started with.
fn write_reply(
    stream: &mut TcpStream,
    mode: ReplyMode,
    message: Value,
    headers: HashMap<String, String>,
) -> bool {
    match mode {
        ReplyMode::Json => write_raw(
            stream,
            200,
            "application/json",
            &message.to_string(),
            &headers,
        ),
        ReplyMode::EventStream => {
            // A progress notification precedes the reply so that a client
            // returning the first event rather than the matching one fails.
            let body = format!(
                "event: message\ndata: {}\n\nevent: message\ndata: {}\n\n",
                json!({"jsonrpc": "2.0", "method": "notifications/progress",
                       "params": {"progressToken": 1, "progress": 1}}),
                message,
            );
            write_raw(stream, 200, "text/event-stream", &body, &headers)
        }
    }
}

/// Writes one HTTP response with an explicit `Content-Length`.
fn write_raw(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
    headers: &HashMap<String, String>,
) -> bool {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        _ => "Error",
    };

    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");

    stream.write_all(head.as_bytes()).is_ok()
        && stream.write_all(body.as_bytes()).is_ok()
        && stream.flush().is_ok()
}
