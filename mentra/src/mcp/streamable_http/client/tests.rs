use super::*;
use crate::mcp::streamable_http::testing::{ReplyMode, ServerBehavior, StreamableHttpFixture};

/// Connects to a fixture, failing the test with the transport's own error.
async fn connect(fixture: &StreamableHttpFixture) -> McpStreamableHttpClient {
    let config = McpStreamableHttpServerConfig::new("fixture", fixture.endpoint());
    McpStreamableHttpClient::connect(&config)
        .await
        .expect("the handshake should complete")
}

#[tokio::test]
async fn the_whole_conversation_goes_to_the_one_configured_endpoint() {
    // This is the defining property of the transport: no second URL is
    // discovered, named by the server, or derived. Everything is a POST to
    // exactly what the operator configured.
    let fixture = StreamableHttpFixture::start(ReplyMode::Json);
    let client = connect(&fixture).await;

    client
        .call_tool("echo", Some(serde_json::json!({"text": "hi"})))
        .await
        .expect("the tool call should succeed");

    let requests = fixture.requests();
    assert!(
        !requests.is_empty(),
        "the fixture should have seen requests"
    );
    for request in &requests {
        assert_eq!(request.method, "POST", "every message is a POST");
        assert_eq!(request.target, "/mcp", "every message goes to the endpoint");
    }
    assert_eq!(
        fixture.rpc_methods(),
        vec![
            "initialize",
            "notifications/initialized",
            "tools/list",
            "tools/call"
        ],
    );
}

#[tokio::test]
async fn every_request_offers_both_json_and_an_event_stream() {
    // The failure that made streamable-only servers unreachable was a 406:
    // mentra had nothing that sent this header. A server picks the framing per
    // reply, so a request that offers only one type can be refused outright.
    let fixture = StreamableHttpFixture::start(ReplyMode::Json);
    let client = connect(&fixture).await;
    client
        .call_tool("echo", None)
        .await
        .expect("the tool call should succeed");

    for request in fixture.requests() {
        let accept = request
            .header("accept")
            .expect("every request should send Accept");
        assert!(
            accept.contains("application/json"),
            "Accept must offer JSON, got {accept:?}"
        );
        assert!(
            accept.contains("text/event-stream"),
            "Accept must offer an event stream, got {accept:?}"
        );
    }
}

#[tokio::test]
async fn a_reply_arriving_as_one_json_body_is_read() {
    let fixture = StreamableHttpFixture::start(ReplyMode::Json);
    let client = connect(&fixture).await;

    assert_eq!(client.tools().len(), 1);
    assert_eq!(client.tools()[0].name, "echo");
    assert_eq!(
        client.server_info().map(|info| info.version.as_str()),
        Some("9.9.9")
    );
}

#[tokio::test]
async fn a_reply_arriving_as_an_event_stream_is_read() {
    // Same conversation, the other framing. The fixture puts a progress
    // notification ahead of every reply, so a client that returns the first
    // event on the stream instead of the one matching its request id gets the
    // notification and fails to parse it as a result.
    let fixture = StreamableHttpFixture::start(ReplyMode::EventStream);
    let client = connect(&fixture).await;

    assert_eq!(client.tools().len(), 1);
    assert_eq!(client.tools()[0].name, "echo");

    let result = client
        .call_tool("echo", Some(serde_json::json!({"text": "hi"})))
        .await
        .expect("the tool call should succeed");
    assert!(!result.is_error);
    assert_eq!(result.content[0].text.as_deref(), Some("echoed"));
}

#[tokio::test]
async fn the_assigned_session_rides_on_every_later_request() {
    let fixture =
        StreamableHttpFixture::start_with(ReplyMode::Json, ServerBehavior::with_session("sess-42"));
    let client = connect(&fixture).await;

    assert_eq!(client.session_id().as_deref(), Some("sess-42"));

    let requests = fixture.requests();
    let (first, rest) = requests.split_first().expect("at least one request");
    assert_eq!(first.rpc_method().as_deref(), Some("initialize"));
    assert_eq!(
        first.header("mcp-session-id"),
        None,
        "initialize cannot carry a session the server has not assigned yet"
    );

    assert!(!rest.is_empty(), "the handshake continues after initialize");
    for request in rest {
        assert_eq!(
            request.header("mcp-session-id"),
            Some("sess-42"),
            "every request after initialize carries the session"
        );
    }
}

#[tokio::test]
async fn a_server_that_uses_no_session_is_never_sent_one() {
    // Sessions are optional in the specification. A client that invented a
    // value, or sent an empty header, would be making one up.
    let fixture = StreamableHttpFixture::start(ReplyMode::Json);
    let client = connect(&fixture).await;

    assert_eq!(client.session_id(), None);
    for request in fixture.requests() {
        assert_eq!(request.header("mcp-session-id"), None);
    }
}

#[tokio::test]
async fn the_negotiated_protocol_version_rides_on_later_requests() {
    let fixture = StreamableHttpFixture::start(ReplyMode::Json);
    connect(&fixture).await;

    let requests = fixture.requests();
    let (first, rest) = requests.split_first().expect("at least one request");
    assert_eq!(
        first.header("mcp-protocol-version"),
        None,
        "nothing is negotiated until initialize answers"
    );
    for request in rest {
        assert_eq!(request.header("mcp-protocol-version"), Some("2025-06-18"));
    }
}

#[tokio::test]
async fn a_protocol_version_this_client_cannot_speak_ends_the_handshake() {
    // The reported revision would otherwise ride on every later request as
    // `MCP-Protocol-Version`. Claiming a protocol this code does not implement
    // is not better than refusing to connect, and this particular value --
    // carrying a CRLF -- would be request splitting if it ever reached a
    // header.
    let fixture = StreamableHttpFixture::start_with(
        ReplyMode::Json,
        ServerBehavior {
            protocol_version: Some("2025-06-18\r\nX-Injected: yes".to_string()),
            ..ServerBehavior::default()
        },
    );
    let config = McpStreamableHttpServerConfig::new("fixture", fixture.endpoint());

    let error = McpStreamableHttpClient::connect(&config)
        .await
        .expect_err("an unsupported revision must not be negotiated");
    assert!(
        matches!(
            error,
            McpStreamableHttpError::UnsupportedProtocolVersion { .. }
        ),
        "expected an unsupported revision, got {error:?}"
    );
    assert!(
        !error.to_string().contains("X-Injected"),
        "the server's value must not reach the error text: {error}"
    );

    for request in fixture.requests() {
        assert_eq!(request.header("x-injected"), None);
    }
}

#[tokio::test]
async fn the_older_streamable_revision_is_still_accepted() {
    // Streamable HTTP starts at 2025-03-26. A server negotiating it is not
    // wrong, and refusing it would be exactly the unreachable-server failure
    // this transport exists to remove.
    let fixture = StreamableHttpFixture::start_with(
        ReplyMode::Json,
        ServerBehavior {
            protocol_version: Some("2025-03-26".to_string()),
            ..ServerBehavior::default()
        },
    );
    let client = connect(&fixture).await;

    assert_eq!(client.tools().len(), 1);
    for request in fixture.requests().iter().skip(1) {
        assert_eq!(
            request.header("mcp-protocol-version"),
            Some("2025-03-26"),
            "the negotiated revision is what rides on later requests"
        );
    }
}

#[tokio::test]
async fn a_reply_answering_a_different_request_is_refused() {
    // A reply is identified by its id, never by being the next thing on the
    // wire. Accepting a mismatched id would hand a caller another call's
    // result. For a `tools/call` the refusal is reported as indeterminate
    // rather than as the mismatch itself: the server answered 2xx, so the call
    // reached it and may have run, and "do not retry this" is the part a
    // caller must not lose. The precise diagnostic survives on the handshake
    // methods, which are safe to repeat -- see the test below.
    let fixture = StreamableHttpFixture::start_with(
        ReplyMode::Json,
        ServerBehavior {
            mismatch_reply_id_on: Some("tools/call".to_string()),
            ..ServerBehavior::default()
        },
    );
    let client = connect(&fixture).await;

    let error = client
        .call_tool("echo", None)
        .await
        .expect_err("a reply for another request must not be accepted");
    assert!(
        matches!(error, McpStreamableHttpError::RequestIndeterminate { .. }),
        "expected an indeterminate call, got {error:?}"
    );
}

#[tokio::test]
async fn a_mismatched_reply_to_a_handshake_request_keeps_its_diagnostic() {
    // `tools/list` is idempotent, so there is nothing to be careful about and
    // no reason to blur what went wrong. This is the half of the reply-id check
    // that still names itself.
    let fixture = StreamableHttpFixture::start_with(
        ReplyMode::Json,
        ServerBehavior {
            mismatch_reply_id_on: Some("tools/list".to_string()),
            ..ServerBehavior::default()
        },
    );
    let config = McpStreamableHttpServerConfig::new("fixture", fixture.endpoint());

    let error = McpStreamableHttpClient::connect(&config)
        .await
        .expect_err("a reply for another request must not be accepted");
    assert!(
        matches!(
            error,
            McpStreamableHttpError::MismatchedReplyId {
                method: "tools/list"
            }
        ),
        "expected a mismatched reply id, got {error:?}"
    );
}

#[tokio::test]
async fn a_streamed_reply_answering_a_different_request_is_not_accepted() {
    // On the streaming path a mismatched id is skipped rather than returned, so
    // the stream ends without an answer. For a tools/call that is indeterminate:
    // the server may well have run the tool.
    let fixture = StreamableHttpFixture::start_with(
        ReplyMode::EventStream,
        ServerBehavior {
            mismatch_reply_id_on: Some("tools/call".to_string()),
            ..ServerBehavior::default()
        },
    );
    let client = connect(&fixture).await;

    let error = client
        .call_tool("echo", None)
        .await
        .expect_err("a reply for another request must not be accepted");
    assert!(
        matches!(error, McpStreamableHttpError::RequestIndeterminate { .. }),
        "expected an indeterminate call, got {error:?}"
    );
}

#[tokio::test]
async fn a_json_rpc_error_keeps_its_code_and_drops_the_server_text() {
    let fixture = StreamableHttpFixture::start_with(
        ReplyMode::Json,
        ServerBehavior {
            tool_call_fails: true,
            ..ServerBehavior::default()
        },
    );
    let client = connect(&fixture).await;

    let error = client
        .call_tool("echo", None)
        .await
        .expect_err("the server reported a tool failure");
    let McpStreamableHttpError::JsonRpc(reported) = &error else {
        panic!("expected a JSON-RPC error, got {error:?}");
    };
    assert_eq!(reported.code, -32000, "the code is fixed metadata and kept");
    assert!(
        !error.to_string().contains("tool exploded"),
        "server text must never reach an error a log or a model will see: {error}"
    );
}

#[tokio::test]
async fn an_http_rejection_fails_the_handshake_with_its_status() {
    // The 406 a streamable-only server answers a malformed request with, which
    // is what this whole transport exists to stop producing.
    let fixture = StreamableHttpFixture::start_with(
        ReplyMode::Json,
        ServerBehavior {
            http_status: Some(406),
            ..ServerBehavior::default()
        },
    );
    let config = McpStreamableHttpServerConfig::new("fixture", fixture.endpoint());

    let error = McpStreamableHttpClient::connect(&config)
        .await
        .expect_err("a 406 must fail the handshake");
    assert!(
        matches!(
            error,
            McpStreamableHttpError::HttpStatus { status, .. }
                if status == reqwest::StatusCode::NOT_ACCEPTABLE
        ),
        "expected an HTTP 406, got {error:?}"
    );
}

#[tokio::test]
async fn a_configured_header_reaches_the_server_and_never_reaches_a_log() {
    let fixture = StreamableHttpFixture::start(ReplyMode::Json);
    let config = McpStreamableHttpServerConfig::new("fixture", fixture.endpoint())
        .with_bearer_token("super-secret-token");

    let client = McpStreamableHttpClient::connect(&config)
        .await
        .expect("the handshake should complete");

    let requests = fixture.requests();
    assert!(!requests.is_empty());
    for request in &requests {
        assert_eq!(
            request.header("authorization"),
            Some("Bearer super-secret-token"),
            "the credential must reach the server it was configured for"
        );
    }

    let rendered = format!("{client:?}");
    assert!(
        !rendered.contains("super-secret-token"),
        "Debug must not print a credential: {rendered}"
    );
    let rendered_config = format!("{config:?}");
    assert!(
        !rendered_config.contains("super-secret-token"),
        "Debug must not print a credential: {rendered_config}"
    );
}

#[tokio::test]
async fn shutdown_ends_a_session_the_server_established() {
    let fixture =
        StreamableHttpFixture::start_with(ReplyMode::Json, ServerBehavior::with_session("sess-7"));
    let client = connect(&fixture).await;

    client.shutdown().await;

    let deletes: Vec<_> = fixture
        .requests()
        .into_iter()
        .filter(|request| request.method == "DELETE")
        .collect();
    assert_eq!(deletes.len(), 1, "shutdown should terminate the session");
    assert_eq!(deletes[0].target, "/mcp");
    assert_eq!(deletes[0].header("mcp-session-id"), Some("sess-7"));
}

#[tokio::test]
async fn shutdown_sends_nothing_when_there_is_no_session_to_end() {
    let fixture = StreamableHttpFixture::start(ReplyMode::Json);
    let client = connect(&fixture).await;
    let before = fixture.requests().len();

    client.shutdown().await;

    assert_eq!(
        fixture.requests().len(),
        before,
        "there is no session to terminate, so nothing is sent"
    );
}

#[tokio::test]
async fn a_tool_call_returns_the_server_result() {
    let fixture = StreamableHttpFixture::start(ReplyMode::Json);
    let client = connect(&fixture).await;

    let result = client
        .call_tool("echo", Some(serde_json::json!({"text": "hi"})))
        .await
        .expect("the tool call should succeed");

    assert!(!result.is_error);
    assert_eq!(result.content.len(), 1);
    assert_eq!(result.content[0].text.as_deref(), Some("echoed"));

    let call = fixture
        .requests()
        .into_iter()
        .find(|request| request.rpc_method().as_deref() == Some("tools/call"))
        .expect("the call should have reached the server");
    let body: serde_json::Value =
        serde_json::from_str(&call.body).expect("the call body should be JSON");
    assert_eq!(body["params"]["name"], "echo");
    assert_eq!(body["params"]["arguments"]["text"], "hi");
}

#[tokio::test]
async fn an_unreachable_endpoint_fails_rather_than_hanging() {
    // Port 1 on loopback refuses immediately on every platform this runs on.
    let config = McpStreamableHttpServerConfig::new("unreachable", "http://127.0.0.1:1/mcp");

    let error = McpStreamableHttpClient::connect(&config)
        .await
        .expect_err("an unreachable endpoint must fail");
    assert!(
        matches!(error, McpStreamableHttpError::Transport(_)),
        "expected a transport failure, got {error:?}"
    );
}

/// Limits small enough that a stalled server fails a test in milliseconds
/// rather than sitting out the shipped minute-scale deadlines.
fn brisk_limits() -> McpStreamableHttpLimits {
    McpStreamableHttpLimits {
        connect_timeout: Duration::from_millis(500),
        initialize_timeout: Duration::from_millis(300),
        list_tools_timeout: Duration::from_millis(300),
        call_tool_timeout: Duration::from_millis(300),
        stream_idle_timeout: Duration::from_millis(200),
        ..McpStreamableHttpLimits::default()
    }
}

#[tokio::test]
async fn a_stalled_notification_body_cannot_hang_the_handshake() {
    // `send()` resolves on the response head and the body streams lazily, so a
    // server that promises a body and then goes quiet stalls anything reading
    // that body without a deadline of its own. This ran inside `connect`,
    // which `RuntimeBuilder::build_async` calls serially with no timeout above
    // it -- so one such server hung the entire runtime build.
    let fixture = StreamableHttpFixture::start_with(
        ReplyMode::Json,
        ServerBehavior {
            stall_after_headers_on: Some("notifications/initialized".to_string()),
            ..ServerBehavior::default()
        },
    );
    let config = McpStreamableHttpServerConfig::new("fixture", fixture.endpoint())
        .with_limits(brisk_limits());

    // The outer bound is the assertion: without one, a regression here does not
    // fail the suite, it hangs it.
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        McpStreamableHttpClient::connect(&config),
    )
    .await
    .expect("connect must observe its own deadline rather than hang");

    let error = outcome.expect_err("a server that never sends the body must fail the handshake");
    assert!(
        matches!(error, McpStreamableHttpError::Timeout(_)),
        "expected a timeout, got {error:?}"
    );
}

#[tokio::test]
async fn a_stalled_json_reply_reports_the_call_as_indeterminate() {
    // The server took the call and answered 200 with a Content-Length, then
    // stopped. The tool may well have run, and the reply framing the server
    // happened to pick must not change that answer.
    let fixture = StreamableHttpFixture::start_with(
        ReplyMode::Json,
        ServerBehavior {
            stall_after_headers_on: Some("tools/call".to_string()),
            ..ServerBehavior::default()
        },
    );
    let config = McpStreamableHttpServerConfig::new("fixture", fixture.endpoint())
        .with_limits(brisk_limits());
    let client = McpStreamableHttpClient::connect(&config)
        .await
        .expect("the handshake should complete");

    let error = tokio::time::timeout(Duration::from_secs(10), client.call_tool("echo", None))
        .await
        .expect("the call must observe its own deadline rather than hang")
        .expect_err("a reply that never arrives is not a success");

    assert!(
        matches!(error, McpStreamableHttpError::RequestIndeterminate { .. }),
        "a call whose reply was lost may have run; got {error:?}"
    );
}

#[tokio::test]
async fn a_server_error_after_the_call_was_accepted_is_indeterminate() {
    // A 5xx can be produced after the application has already run the tool.
    let fixture = StreamableHttpFixture::start_with(
        ReplyMode::Json,
        ServerBehavior {
            tool_call_status: Some(500),
            ..ServerBehavior::default()
        },
    );
    let client = connect(&fixture).await;

    let error = client
        .call_tool("echo", None)
        .await
        .expect_err("a 500 is not a successful call");
    assert!(
        matches!(error, McpStreamableHttpError::RequestIndeterminate { .. }),
        "a 5xx can follow a tool that already ran; got {error:?}"
    );
}

#[tokio::test]
async fn a_client_error_on_the_call_is_a_definite_rejection() {
    // The other half of the split: a 4xx is refused before dispatch, so the
    // caller can retry without risking a second execution.
    let fixture = StreamableHttpFixture::start_with(
        ReplyMode::Json,
        ServerBehavior {
            tool_call_status: Some(401),
            ..ServerBehavior::default()
        },
    );
    let client = connect(&fixture).await;

    let error = client
        .call_tool("echo", None)
        .await
        .expect_err("a 401 is not a successful call");
    assert!(
        matches!(
            error,
            McpStreamableHttpError::HttpStatus { status, .. }
                if status == reqwest::StatusCode::UNAUTHORIZED
        ),
        "a 4xx is refused before dispatch; got {error:?}"
    );
}

#[tokio::test]
async fn a_server_reported_tool_failure_stays_a_definite_answer() {
    // The classification must not swallow the one case that *is* an answer: a
    // JSON-RPC error means the tool ran and reported failure. Reporting that as
    // indeterminate would tell a caller to treat a completed failure as a
    // maybe.
    let fixture = StreamableHttpFixture::start_with(
        ReplyMode::Json,
        ServerBehavior {
            tool_call_fails: true,
            ..ServerBehavior::default()
        },
    );
    let client = connect(&fixture).await;

    let error = client
        .call_tool("echo", None)
        .await
        .expect_err("the server reported a tool failure");
    assert!(
        matches!(error, McpStreamableHttpError::JsonRpc(_)),
        "a reported failure is a definite answer; got {error:?}"
    );
}

#[tokio::test]
async fn a_tool_list_exactly_at_the_page_limit_is_accepted() {
    // The cap says how many pages may be followed, so a list that is exactly
    // that long is within it. Checking the count before asking for another
    // page rejected a server for reaching a limit it was allowed to reach --
    // and made `max_tool_pages: 1` refuse every server alive.
    let fixture = StreamableHttpFixture::start_with(
        ReplyMode::Json,
        ServerBehavior {
            tool_pages: Some(3),
            ..ServerBehavior::default()
        },
    );
    let config = McpStreamableHttpServerConfig::new("fixture", fixture.endpoint()).with_limits(
        McpStreamableHttpLimits {
            max_tool_pages: 3,
            ..McpStreamableHttpLimits::default()
        },
    );

    let client = McpStreamableHttpClient::connect(&config)
        .await
        .expect("a list exactly at the page limit is within it");
    assert_eq!(client.tools().len(), 3, "every page should be collected");
}

#[tokio::test]
async fn a_single_page_limit_still_admits_a_single_page_server() {
    // The degenerate case the off-by-one made impossible.
    let fixture = StreamableHttpFixture::start(ReplyMode::Json);
    let config = McpStreamableHttpServerConfig::new("fixture", fixture.endpoint()).with_limits(
        McpStreamableHttpLimits {
            max_tool_pages: 1,
            ..McpStreamableHttpLimits::default()
        },
    );

    let client = McpStreamableHttpClient::connect(&config)
        .await
        .expect("one page is not more than one page");
    assert_eq!(client.tools().len(), 1);
}

#[tokio::test]
async fn a_server_that_never_stops_paginating_is_refused() {
    // A cursor is opaque, so a server repeating one cannot be caught by value.
    // The page cap is the only thing that ends this walk.
    let fixture = StreamableHttpFixture::start_with(
        ReplyMode::Json,
        ServerBehavior {
            paginates_forever: true,
            ..ServerBehavior::default()
        },
    );
    let config = McpStreamableHttpServerConfig::new("fixture", fixture.endpoint()).with_limits(
        McpStreamableHttpLimits {
            max_tool_pages: 4,
            ..McpStreamableHttpLimits::default()
        },
    );

    let error = McpStreamableHttpClient::connect(&config)
        .await
        .expect_err("an endless walk must be stopped");
    assert!(
        matches!(error, McpStreamableHttpError::TooManyToolPages { limit: 4 }),
        "expected the page cap to stop it, got {error:?}"
    );
}

#[tokio::test]
async fn a_server_advertising_more_tools_than_the_cap_is_refused() {
    // Each page was bounded and the page count was bounded; the list they
    // accumulate into was not.
    let fixture = StreamableHttpFixture::start_with(
        ReplyMode::Json,
        ServerBehavior {
            paginates_forever: true,
            ..ServerBehavior::default()
        },
    );
    let config = McpStreamableHttpServerConfig::new("fixture", fixture.endpoint()).with_limits(
        McpStreamableHttpLimits {
            max_tools: 2,
            ..McpStreamableHttpLimits::default()
        },
    );

    let error = McpStreamableHttpClient::connect(&config)
        .await
        .expect_err("an unbounded tool list must be stopped");
    assert!(
        matches!(error, McpStreamableHttpError::TooManyTools { limit: 2 }),
        "expected the total-tools cap to stop it, got {error:?}"
    );
}

#[tokio::test]
async fn a_session_id_outside_visible_ascii_is_not_adopted() {
    // The specification restricts a session id to 0x21-0x7E. A value carrying
    // a space or obs-text is one an intermediary may read differently than we
    // do, so it is dropped rather than echoed on every later request.
    let fixture =
        StreamableHttpFixture::start_with(ReplyMode::Json, ServerBehavior::with_session("sess 42"));
    let client = connect(&fixture).await;

    assert_eq!(
        client.session_id(),
        None,
        "a session id outside visible ASCII must not be adopted"
    );
    for request in fixture.requests().iter().skip(1) {
        assert_eq!(request.header("mcp-session-id"), None);
    }
}

#[tokio::test]
async fn shutdown_forgets_the_session_it_ended() {
    // Keeping the id would let a second shutdown re-DELETE a session that is
    // already gone, and would let a later call present credentials for one
    // that no longer exists.
    let fixture =
        StreamableHttpFixture::start_with(ReplyMode::Json, ServerBehavior::with_session("sess-9"));
    let client = connect(&fixture).await;

    client.shutdown().await;
    assert_eq!(client.session_id(), None, "the session is gone");

    let after_first = fixture.requests().len();
    client.shutdown().await;
    assert_eq!(
        fixture.requests().len(),
        after_first,
        "a second shutdown has nothing left to end"
    );
}

#[tokio::test]
async fn a_forgotten_session_is_replaced_and_the_call_goes_through() {
    // A server may expire a session at any time. Before this, the 404 came
    // back as a bare error and every tool from that server stayed broken for
    // the runtime's whole life -- one expiry was permanent.
    let fixture = StreamableHttpFixture::start_with(
        ReplyMode::Json,
        ServerBehavior {
            session_id: Some("sess".to_string()),
            rotate_session: true,
            expire_first_tool_call: true,
            ..ServerBehavior::default()
        },
    );
    let client = connect(&fixture).await;
    assert_eq!(client.session_id().as_deref(), Some("sess-1"));

    let result = client
        .call_tool("echo", None)
        .await
        .expect("the call should survive the server forgetting the session");
    assert_eq!(result.content[0].text.as_deref(), Some("echoed"));

    assert_eq!(
        client.session_id().as_deref(),
        Some("sess-2"),
        "the client should be holding the replacement session"
    );

    // The specification requires the replacement handshake to carry no
    // session: sending the forgotten one to a server that just rejected it is
    // the one thing that cannot work.
    let initializes: Vec<_> = fixture
        .requests()
        .into_iter()
        .filter(|request| request.rpc_method().as_deref() == Some("initialize"))
        .collect();
    assert_eq!(
        initializes.len(),
        2,
        "the session should have been replaced"
    );
    assert_eq!(
        initializes[1].header("mcp-session-id"),
        None,
        "a replacement initialize must not carry the forgotten session"
    );

    // The retry is safe precisely because the 404 was a refusal before
    // dispatch, so the tool ran exactly once despite two POSTs.
    let calls = fixture
        .requests()
        .into_iter()
        .filter(|request| request.rpc_method().as_deref() == Some("tools/call"))
        .count();
    assert_eq!(calls, 2, "one rejected before dispatch, one that ran");
}

#[tokio::test]
async fn a_server_that_rejects_every_session_stops_rather_than_looping() {
    // Recovery re-handshakes and re-sends once. If that re-send could itself
    // start another recovery, this server -- which rejects every call that
    // presents a session -- would keep it going forever. The replacement
    // handshake runs through the non-recovering path, so it cannot.
    let fixture = StreamableHttpFixture::start_with(
        ReplyMode::Json,
        ServerBehavior {
            session_id: Some("sess".to_string()),
            rotate_session: true,
            expire_every_tool_call: true,
            ..ServerBehavior::default()
        },
    );
    let client = connect(&fixture).await;

    let error = tokio::time::timeout(Duration::from_secs(10), client.call_tool("echo", None))
        .await
        .expect("a repeated expiry must terminate rather than loop")
        .expect_err("every call is rejected, so none can succeed");
    assert!(
        matches!(error, McpStreamableHttpError::SessionExpired),
        "expected the second rejection to be reported, got {error:?}"
    );

    // Exactly one replacement handshake: the original plus one recovery.
    let initializes = fixture
        .requests()
        .into_iter()
        .filter(|request| request.rpc_method().as_deref() == Some("initialize"))
        .count();
    assert_eq!(initializes, 2, "recovery must be attempted exactly once");
}
