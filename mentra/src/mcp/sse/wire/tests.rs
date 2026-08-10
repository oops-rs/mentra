//! Tests for the incremental Server-Sent Events wire parser.

use super::{SseEvent, SseParser, SseWireError};

/// Feed a whole payload as one chunk and collect every dispatched event.
fn parse_all(payload: &str) -> Vec<SseEvent> {
    let mut parser = SseParser::new(64 * 1024);
    parser
        .feed(payload.as_bytes())
        .expect("payload should parse")
}

#[test]
fn dispatches_a_simple_message_event() {
    let events = parse_all("event: message\ndata: hello\n\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, "message");
    assert_eq!(events[0].data, "hello");
}

#[test]
fn defaults_the_event_name_to_message_when_absent() {
    let events = parse_all("data: hello\n\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, "message");
    assert_eq!(events[0].data, "hello");
}

#[test]
fn parses_the_endpoint_event_name() {
    let events = parse_all("event: endpoint\ndata: /messages/?session_id=abc\n\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, "endpoint");
    assert_eq!(events[0].data, "/messages/?session_id=abc");
}

#[test]
fn accepts_crlf_line_terminators() {
    let events = parse_all("event: message\r\ndata: hello\r\n\r\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, "message");
    assert_eq!(events[0].data, "hello");
}

#[test]
fn accepts_lone_cr_line_terminators() {
    let events = parse_all("event: message\rdata: hello\r\r");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, "message");
    assert_eq!(events[0].data, "hello");
}

#[test]
fn joins_multiple_data_lines_with_newlines() {
    let events = parse_all("event: message\ndata: first\ndata: second\ndata: third\n\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "first\nsecond\nthird");
}

#[test]
fn preserves_embedded_json_across_multiple_data_lines() {
    let events = parse_all("event: message\ndata: {\"jsonrpc\":\"2.0\",\ndata: \"id\":1}\n\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "{\"jsonrpc\":\"2.0\",\n\"id\":1}");
}

#[test]
fn ignores_comment_and_heartbeat_lines() {
    let events = parse_all(": ping\n: keep-alive\nevent: message\ndata: hello\n\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "hello");
}

#[test]
fn ignores_a_standalone_heartbeat_without_dispatching() {
    let events = parse_all(": heartbeat\n\n");
    assert!(events.is_empty());
}

#[test]
fn strips_only_one_leading_space_from_a_field_value() {
    let events = parse_all("data:  two-spaces\n\n");
    assert_eq!(events[0].data, " two-spaces");
}

#[test]
fn accepts_a_data_field_with_no_space_after_the_colon() {
    let events = parse_all("data:hello\n\n");
    assert_eq!(events[0].data, "hello");
}

#[test]
fn treats_a_bare_field_name_as_an_empty_value() {
    let events = parse_all("data\ndata: hello\n\n");
    assert_eq!(events[0].data, "\nhello");
}

#[test]
fn ignores_unknown_fields() {
    let events = parse_all("id: 42\nretry: 3000\nfoo: bar\ndata: hello\n\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "hello");
}

#[test]
fn does_not_dispatch_an_event_without_data() {
    let events = parse_all("event: message\n\n");
    assert!(events.is_empty());
}

#[test]
fn resets_the_event_name_between_dispatches() {
    let events = parse_all("event: endpoint\ndata: /messages\n\ndata: hello\n\n");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event, "endpoint");
    assert_eq!(events[1].event, "message");
}

#[test]
fn dispatches_several_events_from_one_chunk() {
    let events = parse_all("data: one\n\ndata: two\n\ndata: three\n\n");
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].data, "one");
    assert_eq!(events[1].data, "two");
    assert_eq!(events[2].data, "three");
}

#[test]
fn strips_a_leading_utf8_byte_order_mark() {
    let mut parser = SseParser::new(64 * 1024);
    let mut payload = vec![0xEF, 0xBB, 0xBF];
    payload.extend_from_slice(b"data: hello\n\n");
    let events = parser.feed(&payload).expect("payload should parse");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "hello");
}

#[test]
fn reassembles_an_event_split_across_arbitrary_chunks() {
    let payload = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1}\n\n";
    let bytes = payload.as_bytes();
    // Split at every possible byte boundary; the parse must be identical.
    for split in 1..bytes.len() {
        let mut parser = SseParser::new(64 * 1024);
        let mut events = parser
            .feed(&bytes[..split])
            .expect("first half should parse");
        events.extend(
            parser
                .feed(&bytes[split..])
                .expect("second half should parse"),
        );
        assert_eq!(
            events.len(),
            1,
            "split at {split} should dispatch one event"
        );
        assert_eq!(events[0].event, "message");
        assert_eq!(events[0].data, "{\"jsonrpc\":\"2.0\",\"id\":1}");
    }
}

#[test]
fn reassembles_a_crlf_event_split_between_the_cr_and_the_lf() {
    let payload = "data: hello\r\n\r\n";
    let bytes = payload.as_bytes();
    let split = payload.find('\r').expect("payload has a CR") + 1;
    let mut parser = SseParser::new(64 * 1024);
    let mut events = parser
        .feed(&bytes[..split])
        .expect("first half should parse");
    assert!(events.is_empty(), "a dangling CR must not dispatch yet");
    events.extend(
        parser
            .feed(&bytes[split..])
            .expect("second half should parse"),
    );
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "hello");
}

#[test]
fn reassembles_a_multibyte_character_split_across_chunks() {
    let payload = "data: caf\u{e9}\n\n";
    let bytes = payload.as_bytes();
    // The 'é' is two bytes; split between them.
    let split = bytes
        .iter()
        .position(|byte| *byte == 0xC3)
        .expect("payload has a two-byte character")
        + 1;
    let mut parser = SseParser::new(64 * 1024);
    let mut events = parser
        .feed(&bytes[..split])
        .expect("first half should parse");
    events.extend(
        parser
            .feed(&bytes[split..])
            .expect("second half should parse"),
    );
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "caf\u{e9}");
}

#[test]
fn feeding_one_byte_at_a_time_matches_a_single_chunk() {
    let payload = "event: endpoint\r\ndata: /messages/?session_id=abc\r\n\r\ndata: tail\n\n";
    let mut parser = SseParser::new(64 * 1024);
    let mut events = Vec::new();
    for byte in payload.as_bytes() {
        events.extend(
            parser
                .feed(std::slice::from_ref(byte))
                .expect("byte should parse"),
        );
    }
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event, "endpoint");
    assert_eq!(events[0].data, "/messages/?session_id=abc");
    assert_eq!(events[1].event, "message");
    assert_eq!(events[1].data, "tail");
}

#[test]
fn rejects_an_event_larger_than_the_configured_limit() {
    let mut parser = SseParser::new(64);
    let oversized = format!("data: {}\n\n", "x".repeat(512));
    let error = parser
        .feed(oversized.as_bytes())
        .expect_err("oversized event must be rejected");
    assert!(matches!(
        error,
        SseWireError::EventTooLarge { limit: 64, .. }
    ));
}

#[test]
fn rejects_an_unterminated_line_larger_than_the_configured_limit() {
    let mut parser = SseParser::new(64);
    // No terminator at all: the parser must not buffer without bound.
    let error = parser
        .feed("x".repeat(512).as_bytes())
        .expect_err("oversized line must be rejected");
    assert!(matches!(
        error,
        SseWireError::EventTooLarge { limit: 64, .. }
    ));
}

#[test]
fn rejects_an_event_that_only_exceeds_the_limit_across_several_data_lines() {
    let mut parser = SseParser::new(128);
    let line = format!("data: {}\n", "x".repeat(60));
    let mut error = None;
    for _ in 0..10 {
        if let Err(e) = parser.feed(line.as_bytes()) {
            error = Some(e);
            break;
        }
    }
    assert!(
        matches!(error, Some(SseWireError::EventTooLarge { limit: 128, .. })),
        "accumulated data across lines must be bounded, got {error:?}"
    );
}

#[test]
fn counts_the_stored_event_name_toward_the_event_limit() {
    let mut parser = SseParser::new(64);
    let event_name = "x".repeat(57);

    parser
        .feed(format!("event: {event_name}\n").as_bytes())
        .expect("the event line exactly fills the limit");
    let error = parser
        .feed(b"data: xx")
        .expect_err("stored event name and current data line must share the limit");

    assert_eq!(
        error,
        SseWireError::EventTooLarge {
            limit: 64,
            observed: 65,
        }
    );
}

#[test]
fn accepts_an_event_whose_total_buffered_size_exactly_matches_the_limit() {
    let mut parser = SseParser::new(64);
    let event_name = "x".repeat(57);
    let payload = format!("event: {event_name}\ndata: x\n\n");

    let events = parser
        .feed(payload.as_bytes())
        .expect("an event at the exact byte limit should parse");

    assert_eq!(
        events,
        vec![SseEvent {
            event: event_name,
            data: "x".to_string(),
        }]
    );
}

#[test]
fn accounts_size_per_event_rather_than_per_stream() {
    let mut parser = SseParser::new(64);
    // Each event is small; many of them in sequence must not trip the limit.
    for _ in 0..50 {
        let events = parser
            .feed(b"data: small\n\n")
            .expect("each small event should parse");
        assert_eq!(events.len(), 1);
    }
}

#[test]
fn rejects_invalid_utf8_in_the_stream() {
    let mut parser = SseParser::new(64 * 1024);
    let error = parser
        .feed(&[b'd', b'a', b't', b'a', b':', b' ', 0xFF, 0xFE, b'\n', b'\n'])
        .expect_err("invalid UTF-8 must be rejected");
    assert!(matches!(error, SseWireError::InvalidUtf8));
}

#[test]
fn does_not_dispatch_a_trailing_event_without_a_blank_line() {
    // A stream that ends mid-event must not yield a truncated event.
    let events = parse_all("data: complete\n\ndata: incomplete\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "complete");
}
