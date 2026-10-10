//! Server-sent events: how both bindings stream, and the stream a caller
//! reads.
//!
//! Both bindings answer a streaming method with `text/event-stream`, one A2A
//! `StreamResponse` per event's `data`. JSON-RPC wraps each in a response
//! envelope carrying the request's id; HTTP+JSON sends it bare. The framing
//! is parsed here by hand — it is a few rules about lines — so that every
//! event is bounded in size before it is buffered, and the stream in time
//! between any two bytes.

#[cfg(test)]
mod tests;

use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use a2a_lf::{JsonRpcError, JsonRpcResponse, StreamResponse};
use futures_util::Stream;

use super::{jsonrpc, rest};
use crate::a2a::config::A2aLimits;
use crate::a2a::error::{A2aError, A2aRpcError};

/// The media type of an event stream.
pub(super) const EVENT_STREAM: &str = "text/event-stream";

/// The UTF-8 byte order mark the event-stream standard skips at the start.
const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// The events of one streaming call, in the order the agent sent them.
///
/// Every event is about one task: the one the call named, or — for a message
/// that starts a task — the first one an event names; an event about any
/// other is refused as [`A2aError::MismatchedTask`]. The stream ends when the
/// agent closes it, normally once the task reaches a terminal or interrupted
/// state, and after the first error, which is its last item. Dropping it
/// closes the connection.
pub struct A2aEventStream {
    events: Pin<Box<dyn Stream<Item = Result<StreamResponse, A2aError>> + Send>>,
}

impl A2aEventStream {
    pub(super) fn new(
        response: reqwest::Response,
        decoder: Decoder,
        expected_task: Option<String>,
        limits: &A2aLimits,
    ) -> Self {
        let reader = Reader {
            response,
            parser: SseParser::new(limits.max_event_bytes),
            ready: VecDeque::new(),
            decoder,
            expected_task,
            idle_timeout: limits.stream_idle_timeout,
            failure: None,
            ended: false,
            finished: false,
        };
        let events = futures_util::stream::unfold(reader, |mut reader| async move {
            let event = reader.next().await?;
            Some((event, reader))
        });
        Self {
            events: Box::pin(events),
        }
    }
}

impl Stream for A2aEventStream {
    type Item = Result<StreamResponse, A2aError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.events.as_mut().poll_next(cx)
    }
}

impl std::fmt::Debug for A2aEventStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A2aEventStream").finish_non_exhaustive()
    }
}

/// How one stream's events are encoded.
pub(super) struct Decoder {
    pub(super) method: &'static str,
    pub(super) format: EventFormat,
}

pub(super) enum EventFormat {
    /// A JSON-RPC response to the request with this id, per event.
    JsonRpc { id: i64 },
    /// A bare `StreamResponse` per event.
    HttpJson,
}

impl Decoder {
    fn decode(&self, data: &[u8]) -> Result<StreamResponse, A2aError> {
        let method = self.method;
        match self.format {
            EventFormat::JsonRpc { id } => {
                let reply: JsonRpcResponse =
                    serde_json::from_slice(data).map_err(|_| A2aError::Decode { method })?;
                let result = jsonrpc::decode_envelope(method, id, reply)?;
                serde_json::from_value(result).map_err(|_| A2aError::Decode { method })
            }
            EventFormat::HttpJson => decode_bare(method, data),
        }
    }
}

/// An HTTP+JSON event: a `StreamResponse`, or an error in place of one.
///
/// The specification does not say how an error mid-stream is sent over
/// HTTP+JSON, so every shape servers use is read: the binding's own
/// `google.rpc.Status`, wrapped in `{"error": …}` as a reply carries it or
/// bare, and a bare JSON-RPC error object, which the reference server sends.
///
/// A bare status is tried before the JSON-RPC shape, because it has a `code`
/// and a `message` too: read as JSON-RPC, its HTTP-like code would be taken
/// for an A2A one.
fn decode_bare(method: &'static str, data: &[u8]) -> Result<StreamResponse, A2aError> {
    if let Ok(event) = serde_json::from_slice::<StreamResponse>(data) {
        return Ok(event);
    }
    let bare_status = || {
        serde_json::from_slice::<rest::Status>(data)
            .ok()
            .filter(rest::Status::is_status)
    };
    let error = if let Ok(status) = serde_json::from_slice::<rest::StatusEnvelope>(data) {
        status.error.a2a_error()
    } else if let Some(status) = bare_status() {
        status.a2a_error()
    } else if let Ok(error) = serde_json::from_slice::<JsonRpcError>(data) {
        Some(A2aRpcError::from_code(error.code))
    } else {
        return Err(A2aError::Decode { method });
    };
    Err(match error {
        Some(error) => A2aError::Rpc { method, error },
        None => A2aError::Transport("the event stream reported an error"),
    })
}

/// Reads one stream's events, bounded in size and idle time.
struct Reader {
    response: reqwest::Response,
    parser: SseParser,
    /// Events parsed and not yet handed out.
    ready: VecDeque<Vec<u8>>,
    decoder: Decoder,
    expected_task: Option<String>,
    idle_timeout: Duration,
    /// An error the reader met after the events in `ready`, handed out once
    /// they have been.
    failure: Option<A2aError>,
    /// The body has been read to its end.
    ended: bool,
    /// Nothing more will be handed out.
    finished: bool,
}

impl Reader {
    async fn next(&mut self) -> Option<Result<StreamResponse, A2aError>> {
        loop {
            if self.finished {
                return None;
            }
            if let Some(data) = self.ready.pop_front() {
                // An event with no data — a bare `data:` line — carries no
                // `StreamResponse`, so it is skipped rather than refused.
                if data.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let event = self
                    .decoder
                    .decode(&data)
                    .and_then(|event| self.expect_task(event));
                return Some(self.finish_on_error(event));
            }
            if let Some(error) = self.failure.take() {
                return Some(self.finish_on_error(Err(error)));
            }
            if self.ended {
                self.finished = true;
                return None;
            }

            let chunk = tokio::time::timeout(self.idle_timeout, self.response.chunk()).await;
            match chunk {
                Err(_) => self.failure = Some(A2aError::StreamIdle(self.idle_timeout)),
                Ok(Err(_)) => {
                    self.failure = Some(A2aError::Transport("the event stream was interrupted"));
                }
                Ok(Ok(Some(chunk))) => {
                    // Events the chunk completed before an error are the
                    // agent's, and are handed out before it.
                    let mut events = Vec::new();
                    self.failure = self.parser.push(&chunk, &mut events).err();
                    self.ready.extend(events);
                }
                Ok(Ok(None)) => {
                    self.ended = true;
                    self.ready.extend(self.parser.finish());
                }
            }
        }
    }

    /// Refuses an event about a task other than the stream's.
    fn expect_task(&mut self, event: StreamResponse) -> Result<StreamResponse, A2aError> {
        let Some(task_id) = event_task_id(&event) else {
            return Ok(event);
        };
        match &self.expected_task {
            Some(expected) if expected != task_id => Err(A2aError::MismatchedTask {
                method: self.decoder.method,
            }),
            Some(_) => Ok(event),
            None => {
                self.expected_task = Some(task_id.to_string());
                Ok(event)
            }
        }
    }

    fn finish_on_error(
        &mut self,
        event: Result<StreamResponse, A2aError>,
    ) -> Result<StreamResponse, A2aError> {
        self.finished |= event.is_err();
        event
    }
}

/// The task an event is about, when it names one.
fn event_task_id(event: &StreamResponse) -> Option<&str> {
    match event {
        StreamResponse::Task(task) => Some(&task.id),
        StreamResponse::Message(message) => message.task_id.as_deref(),
        StreamResponse::StatusUpdate(update) => Some(&update.task_id),
        StreamResponse::ArtifactUpdate(update) => Some(&update.task_id),
    }
}

/// The `text/event-stream` framing, reduced to what A2A uses: each event's
/// `data`, with comments and every other field ignored.
///
/// One UTF-8 byte order mark at the very start of the stream is skipped;
/// lines end in CRLF, LF or CR; a blank line ends an event; `data` lines are
/// joined with LF. The line being read and the event being gathered together
/// are held to `limit` bytes, so an agent that never ends an event cannot
/// grow the buffer without bound.
pub(super) struct SseParser {
    /// How many bytes of a leading byte order mark have been read, until the
    /// stream is known to have one or not.
    bom: Option<usize>,
    line: Vec<u8>,
    data: Vec<u8>,
    has_data: bool,
    /// The last byte was CR, so an LF next ends nothing new.
    after_cr: bool,
    limit: usize,
}

impl SseParser {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            bom: Some(0),
            line: Vec::new(),
            data: Vec::new(),
            has_data: false,
            after_cr: false,
            limit,
        }
    }

    /// Reads a chunk, adding the data of every event it completes to
    /// `events` — those completed before an error included.
    pub(super) fn push(&mut self, chunk: &[u8], events: &mut Vec<Vec<u8>>) -> Result<(), A2aError> {
        for &byte in chunk {
            if let Some(matched) = self.bom {
                if byte == BOM[matched] {
                    self.bom = (matched + 1 < BOM.len()).then_some(matched + 1);
                    continue;
                }
                // Not a byte order mark after all: what was held back is the
                // start of the first line.
                self.bom = None;
                for &held in &BOM[..matched] {
                    self.take(held, events)?;
                }
            }
            self.take(byte, events)?;
        }
        Ok(())
    }

    fn take(&mut self, byte: u8, events: &mut Vec<Vec<u8>>) -> Result<(), A2aError> {
        let after_cr = std::mem::replace(&mut self.after_cr, byte == b'\r');
        match byte {
            b'\n' if after_cr => {}
            b'\r' | b'\n' => self.end_line(events),
            _ => {
                self.line.push(byte);
                if self.line.len() + self.data.len() > self.limit {
                    return Err(A2aError::EventTooLarge { limit: self.limit });
                }
            }
        }
        Ok(())
    }

    /// Ends the stream, returning an event the agent sent whole but never
    /// ended with a blank line.
    ///
    /// The event-stream standard discards such an event; it is kept here so
    /// that a stream cut mid-event surfaces as an event that fails to decode
    /// rather than as a stream that ended cleanly.
    pub(super) fn finish(&mut self) -> Vec<Vec<u8>> {
        let mut events = Vec::new();
        if let Some(matched) = self.bom.take() {
            self.line.extend_from_slice(&BOM[..matched]);
        }
        if !self.line.is_empty() {
            self.end_line(&mut events);
        }
        if self.has_data {
            self.end_line(&mut events);
        }
        events
    }

    fn end_line(&mut self, events: &mut Vec<Vec<u8>>) {
        let line = std::mem::take(&mut self.line);
        if line.is_empty() {
            if std::mem::take(&mut self.has_data) {
                events.push(std::mem::take(&mut self.data));
            }
            return;
        }
        if line.starts_with(b":") {
            return;
        }
        let (field, value) = match line.iter().position(|&byte| byte == b':') {
            Some(colon) => {
                let value = &line[colon + 1..];
                (&line[..colon], value.strip_prefix(b" ").unwrap_or(value))
            }
            None => (&line[..], &[][..]),
        };
        if field == b"data" {
            if self.has_data {
                self.data.push(b'\n');
            }
            self.data.extend_from_slice(value);
            self.has_data = true;
        }
    }
}
