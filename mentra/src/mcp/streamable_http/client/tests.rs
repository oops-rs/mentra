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
async fn a_protocol_version_that_cannot_be_a_header_falls_back_to_our_own() {
    // The reported version is server-controlled text that this client puts
    // into a header on every later request. A newline in it would be request
    // splitting; it is refused rather than forwarded.
    let fixture = StreamableHttpFixture::start_with(
        ReplyMode::Json,
        ServerBehavior {
            protocol_version: Some("2025-06-18\r\nX-Injected: yes".to_string()),
            ..ServerBehavior::default()
        },
    );
    connect(&fixture).await;

    for request in fixture.requests().iter().skip(1) {
        assert_eq!(
            request.header("mcp-protocol-version"),
            Some(PROTOCOL_VERSION),
            "an unusable reported version falls back to the requested one"
        );
        assert_eq!(request.header("x-injected"), None);
    }
}

#[tokio::test]
async fn a_reply_answering_a_different_request_is_refused() {
    // A reply is identified by its id, never by being the next thing on the
    // wire. Accepting a mismatched id would hand a caller another call's result.
    let fixture = StreamableHttpFixture::start_with(
        ReplyMode::Json,
        ServerBehavior {
            mismatch_tool_call_id: true,
            ..ServerBehavior::default()
        },
    );
    let client = connect(&fixture).await;

    let error = client
        .call_tool("echo", None)
        .await
        .expect_err("a reply for another request must not be accepted");
    assert!(
        matches!(
            error,
            McpStreamableHttpError::MismatchedReplyId {
                method: "tools/call"
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
            mismatch_tool_call_id: true,
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
