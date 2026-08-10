//! Incremental Server-Sent Events wire parser.
//!
//! This is a byte-oriented push parser: callers feed arbitrary byte chunks as
//! they arrive from the transport and receive whole dispatched events back. It
//! deliberately buffers bytes rather than strings so that a UTF-8 sequence or a
//! CRLF pair split across two network chunks is reassembled correctly.
//!
//! The framing rules follow the WHATWG `text/event-stream` interpretation:
//!
//! - lines end with `\n`, `\r\n`, or a lone `\r`;
//! - a line beginning with `:` is a comment (servers use these as heartbeats);
//! - `field: value` strips at most one space after the colon;
//! - a line with no colon is a field name with an empty value;
//! - repeated `data` fields are joined with `\n`;
//! - a blank line dispatches the buffered event, and an event with no `data`
//!   field is discarded rather than dispatched;
//! - a leading UTF-8 byte order mark is ignored.
//!
//! Every buffered event is bounded by a caller-supplied limit so a hostile or
//! malfunctioning server cannot force unbounded memory growth. Mentra's tool
//! result limiter runs far too late to protect this parser.

#[cfg(test)]
mod tests;

/// Byte order mark that may prefix the very first line of a stream.
const UTF8_BOM: &str = "\u{feff}";

/// A dispatched Server-Sent Event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    /// The `event:` field, defaulting to `message` when the server omits it.
    pub(crate) event: String,
    /// The joined `data:` field values, without the trailing newline.
    pub(crate) data: String,
}

/// Errors produced while decoding the SSE byte stream.
///
/// Public because it is reachable through
/// [`McpSseError::Wire`](crate::mcp::McpSseError::Wire); the parser itself
/// stays internal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SseWireError {
    #[error("SSE event exceeded the {limit} byte limit (buffered at least {observed} bytes)")]
    EventTooLarge { limit: usize, observed: usize },

    #[error("SSE stream contained invalid UTF-8")]
    InvalidUtf8,
}

/// Incremental parser over the `text/event-stream` framing.
#[derive(Debug)]
pub(crate) struct SseParser {
    /// Bytes of the line currently being accumulated.
    line: Vec<u8>,
    /// Joined `data:` values for the event currently being accumulated.
    data: String,
    /// The `event:` value for the event currently being accumulated.
    event: Option<String>,
    /// Whether the previous byte was a carriage return whose companion line
    /// feed may still arrive in a later chunk.
    pending_cr: bool,
    /// Whether the next completed line is the first of the stream and may
    /// therefore carry a byte order mark.
    at_stream_start: bool,
    /// Maximum bytes buffered for a single event.
    max_event_bytes: usize,
}

impl SseParser {
    /// Creates a parser that rejects any single event larger than
    /// `max_event_bytes`.
    pub(crate) fn new(max_event_bytes: usize) -> Self {
        Self {
            line: Vec::new(),
            data: String::new(),
            event: None,
            pending_cr: false,
            at_stream_start: true,
            max_event_bytes,
        }
    }

    /// Feeds the next chunk of stream bytes and returns every event completed
    /// by it.
    ///
    /// An error leaves the parser poisoned by contract: the caller must tear
    /// the stream down rather than continue feeding it, because a size or
    /// encoding violation means the framing can no longer be trusted.
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, SseWireError> {
        let mut events = Vec::new();

        for byte in bytes {
            let byte = *byte;

            // A carriage return already ended the previous line. If its
            // companion line feed arrives now, it is part of that same
            // terminator and must not end an additional (empty) line.
            if self.pending_cr {
                self.pending_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }

            match byte {
                b'\n' => self.end_line(&mut events)?,
                b'\r' => {
                    self.pending_cr = true;
                    self.end_line(&mut events)?;
                }
                _ => {
                    self.line.push(byte);
                    self.check_bounds()?;
                }
            }
        }

        Ok(events)
    }

    /// Rejects an event whose buffered bytes exceed the configured limit.
    fn check_bounds(&self) -> Result<(), SseWireError> {
        let observed = self
            .event
            .as_ref()
            .map_or(0, String::len)
            .saturating_add(self.data.len())
            .saturating_add(self.line.len());
        if observed > self.max_event_bytes {
            return Err(SseWireError::EventTooLarge {
                limit: self.max_event_bytes,
                observed,
            });
        }
        Ok(())
    }

    /// Consumes the accumulated line, applying it to the pending event.
    fn end_line(&mut self, events: &mut Vec<SseEvent>) -> Result<(), SseWireError> {
        let line = std::mem::take(&mut self.line);
        let line = std::str::from_utf8(&line).map_err(|_| SseWireError::InvalidUtf8)?;

        // Only the very first line of the stream may carry a byte order mark.
        let line = if std::mem::take(&mut self.at_stream_start) {
            line.strip_prefix(UTF8_BOM).unwrap_or(line)
        } else {
            line
        };

        // A blank line dispatches whatever has been accumulated.
        if line.is_empty() {
            if let Some(event) = self.take_event() {
                events.push(event);
            }
            return Ok(());
        }

        // A leading colon marks a comment, which servers use as a heartbeat.
        if line.starts_with(':') {
            return Ok(());
        }

        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            // A line with no colon is a field name with an empty value.
            None => (line, ""),
        };

        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => {
                self.data.push_str(value);
                self.data.push('\n');
                self.check_bounds()?;
            }
            // `id` and `retry` belong to reconnection, which this transport
            // does not implement; every other field is undefined and ignored.
            _ => {}
        }

        Ok(())
    }

    /// Takes the accumulated event, resetting the per-event state.
    ///
    /// Returns `None` when no `data` field was seen, which the specification
    /// requires be discarded rather than dispatched.
    fn take_event(&mut self) -> Option<SseEvent> {
        let event = self.event.take();
        let mut data = std::mem::take(&mut self.data);

        if data.is_empty() {
            return None;
        }

        // The dispatch step drops the single trailing newline added by the
        // last `data` field.
        data.pop();

        Some(SseEvent {
            event: event.unwrap_or_else(|| "message".to_string()),
            data,
        })
    }
}
