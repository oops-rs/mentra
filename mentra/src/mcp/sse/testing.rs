//! A deterministic local HTTP+SSE server for transport tests.
//!
//! The transport needs a fixture that keeps one connection parked on a
//! long-lived `GET` while serving `POST` requests on other connections, and
//! that lets a test decide exactly when each SSE event reaches the client. A
//! raw [`TcpListener`] driven from [`std::thread`] gives that control with no
//! new dependencies, matching the fixtures already used in `mentra-provider`.
//!
//! Two properties keep the resulting tests deterministic rather than
//! timing-dependent:
//!
//! - **A thread per connection.** One accept loop hands each connection to its
//!   own thread, so a blocking read on the parked `GET` cannot stop a `POST`
//!   from being answered.
//! - **Chunked framing with a flush per event.** Chunked encoding is used
//!   rather than read-until-close so that a clean end (`0\r\n\r\n`) and an
//!   abrupt truncation are distinguishable by the client. Without the explicit
//!   flush the operating system coalesces writes and the test would pass even
//!   for a client that buffered the whole body.

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use crate::mcp::testing::{CapturedRequest, read_request};

/// How a fixture answers one JSON-RPC `POST`.
#[derive(Debug, Clone)]
pub(crate) enum PostReply {
    /// Answer `202 Accepted`, the reference servers' behavior.
    Accepted,
    /// Answer `200 OK` with an empty body.
    Ok,
    /// Answer with the given status and body.
    Status { code: u16, body: String },
    /// Answer `307` pointing at another origin, which must not be followed.
    Redirect { location: String },
    /// Close the connection without answering.
    Drop,
    /// Read the complete request, then withhold the response headers.
    StallBeforeHeaders,
    /// Send a successful response head, then withhold its declared body.
    StallAfterHeaders,
}

/// What the fixture should do when the SSE `GET` arrives.
#[derive(Debug, Clone)]
pub(crate) enum StreamOpening {
    /// Accept the stream and serve events on demand.
    Accept,
    /// Answer `200` with a content type that is not `text/event-stream`.
    WrongContentType,
    /// Answer with the given status and body.
    Status { code: u16, body: String },
    /// Answer `307` pointing elsewhere, which must not be followed.
    Redirect { location: String },
}

/// Shared state between the test and the fixture's threads.
struct Shared {
    requests: Mutex<Vec<CapturedRequest>>,
    replies: Mutex<Vec<PostReply>>,
    stream_opened: (Mutex<bool>, Condvar),
    posts_seen: (Mutex<usize>, Condvar),
    post_headers_sent: (Mutex<usize>, Condvar),
    stalled_posts_released: (Mutex<bool>, Condvar),
    connections: AtomicUsize,
}

/// Instruction sent to the thread holding the SSE connection open.
enum StreamCommand {
    /// Write raw bytes as one chunk and flush.
    Write(String),
    /// End the stream cleanly with a terminal chunk.
    Close,
    /// Drop the connection without a terminal chunk, simulating a truncation.
    Abort,
}

/// A running local MCP HTTP+SSE server.
pub(crate) struct SseTestServer {
    base_url: String,
    shared: Arc<Shared>,
    commands: mpsc::Sender<StreamCommand>,
}

impl SseTestServer {
    /// Starts a fixture that accepts the SSE stream and answers every `POST`
    /// with `202 Accepted`.
    pub(crate) fn start() -> Self {
        Self::with_opening(StreamOpening::Accept)
    }

    /// Starts a fixture whose SSE `GET` is answered as described.
    pub(crate) fn with_opening(opening: StreamOpening) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fixture listener");
        let addr = listener.local_addr().expect("read the fixture address");

        let shared = Arc::new(Shared {
            requests: Mutex::new(Vec::new()),
            replies: Mutex::new(Vec::new()),
            stream_opened: (Mutex::new(false), Condvar::new()),
            posts_seen: (Mutex::new(0), Condvar::new()),
            post_headers_sent: (Mutex::new(0), Condvar::new()),
            stalled_posts_released: (Mutex::new(false), Condvar::new()),
            connections: AtomicUsize::new(0),
        });
        let (commands, command_rx) = mpsc::channel();

        let accept_shared = Arc::clone(&shared);
        let command_rx = Arc::new(Mutex::new(command_rx));
        thread::spawn(move || {
            for incoming in listener.incoming() {
                let Ok(stream) = incoming else { break };
                accept_shared.connections.fetch_add(1, Ordering::SeqCst);
                let shared = Arc::clone(&accept_shared);
                let command_rx = Arc::clone(&command_rx);
                let opening = opening.clone();
                thread::spawn(move || serve_connection(stream, shared, command_rx, opening));
            }
        });

        Self {
            base_url: format!("http://{addr}"),
            shared,
            commands,
        }
    }

    /// The URL of the SSE stream endpoint.
    pub(crate) fn sse_url(&self) -> String {
        format!("{}/sse", self.base_url)
    }

    /// The fixture's origin, for building cross-origin cases.
    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Queues the reply used for the next `POST`.
    ///
    /// Replies are consumed in order; once the queue is empty every `POST` is
    /// answered `202 Accepted`.
    pub(crate) fn queue_post_reply(&self, reply: PostReply) {
        self.shared
            .replies
            .lock()
            .expect("lock the reply queue")
            .push(reply);
    }

    /// Blocks until the client has opened the SSE stream.
    pub(crate) fn wait_for_stream(&self) {
        let (lock, condvar) = &self.shared.stream_opened;
        let mut opened = lock.lock().expect("lock the stream flag");
        while !*opened {
            let (guard, timeout) = condvar
                .wait_timeout(opened, Duration::from_secs(10))
                .expect("wait for the stream");
            opened = guard;
            assert!(!timeout.timed_out(), "the client never opened the stream");
        }
    }

    /// Blocks until at least `count` `POST` requests have been captured.
    ///
    /// Tests synchronize on this rather than on a sleep so they stay
    /// deterministic under load.
    pub(crate) fn wait_for_posts(&self, count: usize) {
        let (lock, condvar) = &self.shared.posts_seen;
        let mut seen = lock.lock().expect("lock the post counter");
        while *seen < count {
            let (guard, timeout) = condvar
                .wait_timeout(seen, Duration::from_secs(10))
                .expect("wait for posts");
            seen = guard;
            assert!(
                !timeout.timed_out(),
                "expected {count} POSTs, saw {}",
                *seen
            );
        }
    }

    /// Blocks until at least `count` `POST` response heads have been flushed.
    pub(crate) fn wait_for_post_response_headers(&self, count: usize) {
        let (lock, condvar) = &self.shared.post_headers_sent;
        let mut seen = lock.lock().expect("lock the POST response-head counter");
        while *seen < count {
            let (guard, timeout) = condvar
                .wait_timeout(seen, Duration::from_secs(10))
                .expect("wait for POST response headers");
            seen = guard;
            assert!(
                !timeout.timed_out(),
                "expected {count} POST response heads, saw {}",
                *seen
            );
        }
    }

    /// Releases every fixture connection deliberately stalled while replying.
    pub(crate) fn release_stalled_posts(&self) {
        let (lock, condvar) = &self.shared.stalled_posts_released;
        *lock.lock().expect("lock the stalled-POST gate") = true;
        condvar.notify_all();
    }

    /// Writes raw bytes to the SSE stream as one chunk.
    pub(crate) fn send_raw(&self, payload: impl Into<String>) {
        let _ = self.commands.send(StreamCommand::Write(payload.into()));
    }

    /// Writes an `endpoint` event naming the given POST target.
    pub(crate) fn send_endpoint(&self, target: &str) {
        self.send_raw(format!("event: endpoint\ndata: {target}\n\n"));
    }

    /// Writes a `message` event carrying the given JSON-RPC payload.
    pub(crate) fn send_message(&self, payload: &serde_json::Value) {
        self.send_raw(format!("event: message\ndata: {payload}\n\n"));
    }

    /// Ends the stream cleanly.
    pub(crate) fn close_stream(&self) {
        let _ = self.commands.send(StreamCommand::Close);
    }

    /// Drops the stream connection without a terminal chunk.
    pub(crate) fn abort_stream(&self) {
        let _ = self.commands.send(StreamCommand::Abort);
    }

    /// Every request the fixture has captured, in arrival order.
    pub(crate) fn requests(&self) -> Vec<CapturedRequest> {
        self.shared
            .requests
            .lock()
            .expect("lock the request log")
            .clone()
    }

    /// Only the `POST` requests captured so far.
    pub(crate) fn posts(&self) -> Vec<CapturedRequest> {
        self.requests()
            .into_iter()
            .filter(|request| request.method == "POST")
            .collect()
    }
}

/// Serves one accepted connection until the peer goes away.
fn serve_connection(
    mut stream: TcpStream,
    shared: Arc<Shared>,
    commands: Arc<Mutex<mpsc::Receiver<StreamCommand>>>,
    opening: StreamOpening,
) {
    while let Some(request) = read_request(&mut stream) {
        let is_stream_request = request.method == "GET";
        if !is_stream_request {
            let (lock, condvar) = &shared.posts_seen;
            let mut seen = lock.lock().expect("lock the post counter");
            *seen += 1;
            condvar.notify_all();
        }
        shared
            .requests
            .lock()
            .expect("lock the request log")
            .push(request);

        if is_stream_request {
            serve_stream(&mut stream, &shared, &commands, &opening);
            return;
        }

        let reply = {
            let mut replies = shared.replies.lock().expect("lock the reply queue");
            if replies.is_empty() {
                PostReply::Accepted
            } else {
                replies.remove(0)
            }
        };

        if !write_post_reply(&mut stream, reply, &shared) {
            return;
        }
    }
}

/// Answers the SSE `GET` and then streams events on command.
fn serve_stream(
    stream: &mut TcpStream,
    shared: &Arc<Shared>,
    commands: &Arc<Mutex<mpsc::Receiver<StreamCommand>>>,
    opening: &StreamOpening,
) {
    let head = match opening {
        StreamOpening::Accept => "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream; charset=utf-8\r\ntransfer-encoding: chunked\r\n\r\n".to_string(),
        StreamOpening::WrongContentType => {
            "HTTP/1.1 200 OK\r\ncontent-type: application/x-remote-canary\r\ncontent-length: 2\r\n\r\n{}".to_string()
        }
        StreamOpening::Status { code, body } => format!(
            "HTTP/1.1 {code} Status\r\ncontent-type: text/plain\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        ),
        StreamOpening::Redirect { location } => format!(
            "HTTP/1.1 307 Temporary Redirect\r\nlocation: {location}\r\ncontent-length: 0\r\n\r\n"
        ),
    };

    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    let _ = stream.flush();

    if !matches!(opening, StreamOpening::Accept) {
        return;
    }

    // Only signal readiness once the client can actually receive events.
    let (lock, condvar) = &shared.stream_opened;
    *lock.lock().expect("lock the stream flag") = true;
    condvar.notify_all();

    let commands = commands.lock().expect("lock the command channel");
    while let Ok(command) = commands.recv() {
        match command {
            StreamCommand::Write(payload) => {
                let framed = format!("{:X}\r\n{payload}\r\n", payload.len());
                if stream.write_all(framed.as_bytes()).is_err() {
                    return;
                }
                // Flush per event, or the OS coalesces writes and the test
                // would pass for a client that buffered the whole body.
                let _ = stream.flush();
            }
            StreamCommand::Close => {
                let _ = stream.write_all(b"0\r\n\r\n");
                let _ = stream.flush();
                return;
            }
            StreamCommand::Abort => return,
        }
    }
}

/// Writes one `POST` reply, reporting whether the connection may be reused.
fn write_post_reply(stream: &mut TcpStream, reply: PostReply, shared: &Shared) -> bool {
    let stall_before_headers = matches!(&reply, PostReply::StallBeforeHeaders);
    let stall_after_headers = matches!(&reply, PostReply::StallAfterHeaders);

    if stall_before_headers {
        wait_for_stalled_post_release(shared);
        return false;
    }

    let response = match reply {
        PostReply::Accepted => "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\r\n".to_string(),
        PostReply::Ok => "HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n".to_string(),
        PostReply::Status { code, body } => format!(
            "HTTP/1.1 {code} Status\r\ncontent-type: text/plain\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        ),
        PostReply::Redirect { location } => format!(
            "HTTP/1.1 307 Temporary Redirect\r\nlocation: {location}\r\ncontent-length: 0\r\n\r\n"
        ),
        PostReply::Drop => return false,
        PostReply::StallAfterHeaders => "HTTP/1.1 200 OK\r\ncontent-length: 4\r\n\r\n".to_string(),
        PostReply::StallBeforeHeaders => unreachable!("handled before building the response"),
    };

    if stream.write_all(response.as_bytes()).is_err() {
        return false;
    }
    if stream.flush().is_err() {
        return false;
    }

    let (lock, condvar) = &shared.post_headers_sent;
    *lock.lock().expect("lock the POST response-head counter") += 1;
    condvar.notify_all();

    if stall_after_headers {
        wait_for_stalled_post_release(shared);
        return false;
    }

    true
}

/// Waits until the test explicitly releases a deliberately stalled reply.
fn wait_for_stalled_post_release(shared: &Shared) {
    let (lock, condvar) = &shared.stalled_posts_released;
    let mut released = lock.lock().expect("lock the stalled-POST gate");
    while !*released {
        released = condvar
            .wait(released)
            .expect("wait for the stalled POST to be released");
    }
}
