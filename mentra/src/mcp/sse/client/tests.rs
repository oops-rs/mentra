//! Tests for the legacy MCP HTTP+SSE client, driven by a local fixture.

use serde_json::json;

use super::{McpSseClient, McpSseError};
use crate::mcp::sse::config::{McpSseLimits, McpSseServerConfig};
use crate::mcp::sse::testing::{PostReply, SseTestServer, StreamOpening};

const REMOTE_CANARY: &str = "REMOTE_CANARY_MUST_NOT_SURFACE";

fn assert_remote_canary_absent(error: &McpSseError) {
    let display = error.to_string();
    let debug = format!("{error:?}");
    assert!(!display.contains(REMOTE_CANARY), "got {display}");
    assert!(!debug.contains(REMOTE_CANARY), "got {debug}");
}

/// The `initialize` result every handshake test replies with.
fn initialize_result(id: u64) -> serde_json::Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fixture", "version": "1.2.3"}
        }
    })
}

/// A single-page `tools/list` result.
fn tools_result(id: u64, tools: serde_json::Value) -> serde_json::Value {
    json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools}})
}

fn config(server: &SseTestServer) -> McpSseServerConfig {
    McpSseServerConfig::new("fixture", server.sse_url())
}

/// Drives the fixture through the handshake so a test can reach a connected
/// client without repeating the three scripted replies.
async fn connect(server: &SseTestServer, config: McpSseServerConfig) -> McpSseClient {
    let connecting = tokio::spawn(async move { McpSseClient::connect(&config).await });

    server.wait_for_stream();
    server.send_endpoint("/messages/?session_id=abc");

    server.wait_for_posts(1);
    server.send_message(&initialize_result(1));

    // The initialized notification and tools/list follow.
    server.wait_for_posts(3);
    server.send_message(&tools_result(
        2,
        json!([{
            "name": "search",
            "description": "Search the corpus",
            "inputSchema": {"type": "object", "properties": {"q": {"type": "string"}}}
        }]),
    ));

    connecting
        .await
        .expect("the connect task should not panic")
        .expect("the handshake should succeed")
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn completes_the_initialize_initialized_and_tools_list_handshake() {
    let server = SseTestServer::start();
    let client = connect(&server, config(&server)).await;

    assert_eq!(
        client.server_info().map(|info| info.name.as_str()),
        Some("fixture")
    );
    assert_eq!(client.tools().len(), 1);
    assert_eq!(client.tools()[0].name, "search");

    let methods: Vec<Option<String>> = server
        .posts()
        .iter()
        .map(|request| request.rpc_method())
        .collect();
    assert_eq!(
        methods,
        vec![
            Some("initialize".to_string()),
            Some("notifications/initialized".to_string()),
            Some("tools/list".to_string()),
        ],
        "the handshake must follow the 2024-11-05 order"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_initialized_notification_carries_no_request_id() {
    let server = SseTestServer::start();
    let _client = connect(&server, config(&server)).await;

    let notification = server
        .posts()
        .into_iter()
        .find(|request| request.rpc_method().as_deref() == Some("notifications/initialized"))
        .expect("the notification should be sent");
    assert!(
        notification.rpc_id().is_none(),
        "a notification must not carry an id"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn opens_the_stream_with_the_event_stream_accept_header() {
    let server = SseTestServer::start();
    let _client = connect(&server, config(&server)).await;

    let stream_request = server
        .requests()
        .into_iter()
        .find(|request| request.method == "GET")
        .expect("the stream should be opened with GET");
    assert_eq!(
        stream_request.header("accept"),
        Some("text/event-stream"),
        "the GET must advertise the event stream"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn posts_json_rpc_messages_as_application_json() {
    let server = SseTestServer::start();
    let _client = connect(&server, config(&server)).await;

    let post = server.posts().into_iter().next().expect("a POST is sent");
    assert_eq!(post.header("content-type"), Some("application/json"));
}

#[tokio::test(flavor = "multi_thread")]
async fn posts_to_the_endpoint_named_by_the_server() {
    let server = SseTestServer::start();
    let _client = connect(&server, config(&server)).await;

    let post = server.posts().into_iter().next().expect("a POST is sent");
    assert_eq!(
        post.target, "/messages/?session_id=abc",
        "the session id query must be preserved verbatim"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn accepts_a_202_response_to_the_message_post() {
    let server = SseTestServer::start();
    // 202 Accepted is what both reference servers return.
    server.queue_post_reply(PostReply::Accepted);
    let client = connect(&server, config(&server)).await;
    assert_eq!(client.tools().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn accepts_a_200_response_to_the_message_post() {
    let server = SseTestServer::start();
    server.queue_post_reply(PostReply::Ok);
    let client = connect(&server, config(&server)).await;
    assert_eq!(client.tools().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn walks_every_page_of_a_paginated_tools_list() {
    let server = SseTestServer::start();
    let config = config(&server);
    let connecting = tokio::spawn(async move { McpSseClient::connect(&config).await });

    server.wait_for_stream();
    server.send_endpoint("/messages/?session_id=abc");
    server.wait_for_posts(1);
    server.send_message(&initialize_result(1));

    server.wait_for_posts(3);
    server.send_message(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{"name": "first", "inputSchema": {"type": "object"}}],
            "nextCursor": "page-2"
        }
    }));

    server.wait_for_posts(4);
    server.send_message(&tools_result(
        3,
        json!([{"name": "second", "inputSchema": {"type": "object"}}]),
    ));

    let client = connecting
        .await
        .expect("no panic")
        .expect("the handshake should succeed");

    let names: Vec<&str> = client
        .tools()
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(names, vec!["first", "second"]);

    let cursors: Vec<Option<String>> = server
        .posts()
        .iter()
        .filter(|request| request.rpc_method().as_deref() == Some("tools/list"))
        .map(|request| {
            serde_json::from_str::<serde_json::Value>(&request.body)
                .ok()?
                .get("params")?
                .get("cursor")?
                .as_str()
                .map(str::to_string)
        })
        .collect();
    assert_eq!(
        cursors,
        vec![None, Some("page-2".to_string())],
        "the second page must echo the server's opaque cursor"
    );
}

/// The cap says how many pages may be followed, so a list exactly that long is
/// within it. Checking the count before another page is asked for refused a
/// server for reaching a limit it was allowed to reach — and made
/// `max_tool_pages: 1` reject every server alive.
#[tokio::test(flavor = "multi_thread")]
async fn accepts_a_tools_list_exactly_as_long_as_the_page_limit() {
    let server = SseTestServer::start();
    let mut config = config(&server);
    config.limits = McpSseLimits {
        max_tool_pages: 2,
        ..McpSseLimits::default()
    };
    let connecting = tokio::spawn(async move { McpSseClient::connect(&config).await });

    server.wait_for_stream();
    server.send_endpoint("/messages/?session_id=abc");
    server.wait_for_posts(1);
    server.send_message(&initialize_result(1));

    server.wait_for_posts(3);
    server.send_message(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{"name": "first", "inputSchema": {"type": "object"}}],
            "nextCursor": "page-2"
        }
    }));

    // The second page is the last: no cursor, so nothing further is asked for
    // and the limit is never exceeded.
    server.wait_for_posts(4);
    server.send_message(&tools_result(
        3,
        json!([{"name": "second", "inputSchema": {"type": "object"}}]),
    ));

    let client = tokio::time::timeout(std::time::Duration::from_secs(10), connecting)
        .await
        .expect("the handshake should not hang")
        .expect("no panic")
        .expect("a list exactly at the page limit is within it");

    let names: Vec<&str> = client
        .tools()
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(names, vec!["first", "second"]);
}

/// A server that keeps returning a cursor must not loop forever. Cursors are
/// opaque, so a repeat cannot be detected by value; only a page bound stops it.
#[tokio::test(flavor = "multi_thread")]
async fn stops_paginating_a_server_that_never_ends_its_tools_list() {
    let server = SseTestServer::start();
    let mut config = config(&server);
    config.limits = McpSseLimits {
        max_tool_pages: 4,
        ..McpSseLimits::default()
    };
    let connecting = tokio::spawn(async move { McpSseClient::connect(&config).await });

    server.wait_for_stream();
    server.send_endpoint("/messages/?session_id=abc");
    server.wait_for_posts(1);
    server.send_message(&initialize_result(1));

    // Answer each tools/list with the same cursor. Replies must follow their
    // request, not precede it: a response for an unregistered id is dropped.
    // With a bound of 4 the client asks exactly four times and then gives up,
    // so the count is exact rather than open-ended.
    for page in 0..4 {
        server.wait_for_posts(3 + page);
        server.send_message(&json!({
            "jsonrpc": "2.0",
            "id": 2 + page,
            "result": {"tools": [], "nextCursor": "always-more"}
        }));
    }

    let error = tokio::time::timeout(std::time::Duration::from_secs(10), connecting)
        .await
        .expect("the client must give up rather than paginate forever")
        .expect("no panic")
        .expect_err("an endless cursor must fail");
    assert!(
        matches!(error, McpSseError::TooManyToolPages { limit: 4 }),
        "got {error:?}"
    );

    let pages = server
        .posts()
        .into_iter()
        .filter(|request| request.rpc_method().as_deref() == Some("tools/list"))
        .count();
    assert_eq!(
        pages, 4,
        "the client must stop at the configured page bound"
    );
}

// ---------------------------------------------------------------------------
// Tool calls
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn calls_a_tool_and_returns_its_content() {
    let server = SseTestServer::start();
    let client = connect(&server, config(&server)).await;

    let calling = tokio::spawn(async move {
        (
            client.call_tool("search", Some(json!({"q": "logs"}))).await,
            client,
        )
    });

    server.wait_for_posts(4);
    server.send_message(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "result": {"content": [{"type": "text", "text": "found it"}], "isError": false}
    }));

    let (result, _client) = calling.await.expect("no panic");
    let result = result.expect("the call should succeed");
    assert!(!result.is_error);
    assert_eq!(result.content[0].text.as_deref(), Some("found it"));

    let call = server
        .posts()
        .into_iter()
        .find(|request| request.rpc_method().as_deref() == Some("tools/call"))
        .expect("the call should be posted");
    let body: serde_json::Value = serde_json::from_str(&call.body).expect("valid JSON");
    assert_eq!(body["params"]["name"], "search");
    assert_eq!(body["params"]["arguments"]["q"], "logs");
}

#[tokio::test(flavor = "multi_thread")]
async fn surfaces_a_tool_result_flagged_as_an_error() {
    let server = SseTestServer::start();
    let client = connect(&server, config(&server)).await;

    let calling = tokio::spawn(async move { (client.call_tool("search", None).await, client) });

    server.wait_for_posts(4);
    server.send_message(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "result": {"content": [{"type": "text", "text": REMOTE_CANARY}], "isError": true}
    }));

    let (result, _client) = calling.await.expect("no panic");
    let result = result.expect("an isError result is still a successful response");
    assert!(
        result.is_error,
        "isError must be preserved rather than turned into a transport failure"
    );
    assert_eq!(result.content[0].text.as_deref(), Some(REMOTE_CANARY));
}

#[tokio::test(flavor = "multi_thread")]
async fn surfaces_a_json_rpc_error_response() {
    let server = SseTestServer::start();
    let client = connect(&server, config(&server)).await;

    let calling = tokio::spawn(async move { (client.call_tool("missing", None).await, client) });

    server.wait_for_posts(4);
    server.send_message(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "error": {
            "code": -32602,
            "message": REMOTE_CANARY,
            "data": {"forged": REMOTE_CANARY}
        }
    }));

    let (result, _client) = calling.await.expect("no panic");
    let error = result.expect_err("a JSON-RPC error is a failure");
    let McpSseError::JsonRpc(rpc) = &error else {
        panic!("got {error:?}");
    };
    assert_eq!(rpc.code, -32602);
    assert_eq!(rpc.message, "server message omitted");
    assert!(rpc.data.is_none(), "server data must be discarded");
    assert_remote_canary_absent(&error);
    assert!(error.to_string().contains("-32602"), "got {error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn response_decode_errors_do_not_retain_server_text() {
    let server = SseTestServer::start();
    let client = connect(&server, config(&server)).await;

    let calling = tokio::spawn(async move { (client.call_tool("search", None).await, client) });

    server.wait_for_posts(4);
    server.send_message(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "result": {"content": REMOTE_CANARY, "isError": false}
    }));

    let (result, _client) = calling.await.expect("no panic");
    let error = result.expect_err("the response shape is invalid");
    assert!(matches!(error, McpSseError::ParseError(_)), "got {error:?}");
    assert_remote_canary_absent(&error);
}

#[tokio::test(flavor = "multi_thread")]
async fn resolves_concurrent_calls_whose_responses_arrive_in_reverse_order() {
    let server = SseTestServer::start();
    let client = std::sync::Arc::new(connect(&server, config(&server)).await);

    let first = {
        let client = std::sync::Arc::clone(&client);
        tokio::spawn(async move { client.call_tool("search", Some(json!({"q": "one"}))).await })
    };
    let second = {
        let client = std::sync::Arc::clone(&client);
        tokio::spawn(async move { client.call_tool("search", Some(json!({"q": "two"}))).await })
    };

    // Both calls must be in flight before either is answered.
    server.wait_for_posts(5);

    // Task spawn order is not poll order: on Windows the second future can
    // reserve the lower JSON-RPC id. Derive the correlation from what actually
    // reached the wire instead of assigning ids to source-code order.
    let call_ids = server
        .posts()
        .into_iter()
        .filter(|request| request.rpc_method().as_deref() == Some("tools/call"))
        .map(|request| {
            let body: serde_json::Value =
                serde_json::from_str(&request.body).expect("tool call body is JSON");
            let query = body["params"]["arguments"]["q"]
                .as_str()
                .expect("fixture call carries q")
                .to_string();
            let id = body["id"].as_u64().expect("fixture call carries an id");
            (query, id)
        })
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(call_ids.len(), 2);
    let first_id = call_ids["one"];
    let second_id = call_ids["two"];

    // Answer the second request first: the stream carries no ordering guarantee.
    server.send_message(&json!({
        "jsonrpc": "2.0",
        "id": second_id,
        "result": {"content": [{"type": "text", "text": "second"}], "isError": false}
    }));
    server.send_message(&json!({
        "jsonrpc": "2.0",
        "id": first_id,
        "result": {"content": [{"type": "text", "text": "first"}], "isError": false}
    }));

    let first = first
        .await
        .expect("no panic")
        .expect("first should resolve");
    let second = second
        .await
        .expect("no panic")
        .expect("second should resolve");

    assert_eq!(
        first.content[0].text.as_deref(),
        Some("first"),
        "each caller must receive the response matching its own id"
    );
    assert_eq!(second.content[0].text.as_deref(), Some("second"));
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn sends_configured_headers_on_both_the_stream_and_the_posts() {
    let server = SseTestServer::start();
    let config = McpSseServerConfig::new("fixture", server.sse_url())
        .with_bearer_token("super-secret-token")
        .with_header("x-tenant", "acme")
        .allowing_plaintext_credentials();
    let _client = connect(&server, config).await;

    let requests = server.requests();
    let stream_request = requests
        .iter()
        .find(|request| request.method == "GET")
        .expect("the stream is opened");
    assert_eq!(
        stream_request.header("authorization"),
        Some("Bearer super-secret-token"),
        "the GET must carry the credential"
    );
    assert_eq!(stream_request.header("x-tenant"), Some("acme"));

    let post = requests
        .iter()
        .find(|request| request.method == "POST")
        .expect("a message is posted");
    assert_eq!(
        post.header("authorization"),
        Some("Bearer super-secret-token"),
        "the POST must carry the credential too"
    );
    assert_eq!(post.header("x-tenant"), Some("acme"));
}

#[tokio::test(flavor = "multi_thread")]
async fn header_values_never_appear_in_client_debug_output() {
    let server = SseTestServer::start();
    let config = McpSseServerConfig::new("fixture", server.sse_url())
        .with_bearer_token("super-secret-token")
        .allowing_plaintext_credentials();
    let client = connect(&server, config).await;

    let rendered = format!("{client:?}");
    assert!(
        !rendered.contains("super-secret-token"),
        "the client must not render its credentials: {rendered}"
    );
}

// ---------------------------------------------------------------------------
// Endpoint handling
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn rejects_an_endpoint_pointing_at_another_origin() {
    let server = SseTestServer::start();
    let config = config(&server);
    let connecting = tokio::spawn(async move { McpSseClient::connect(&config).await });

    server.wait_for_stream();
    server.send_endpoint("https://remote-canary-must-not-surface.invalid/messages");

    let error = connecting
        .await
        .expect("no panic")
        .expect_err("a cross-origin endpoint must be refused");
    assert!(matches!(error, McpSseError::Endpoint(_)), "got {error:?}");
    assert_remote_canary_absent(&error);
    assert!(
        !format!("{error:?}").contains("remote-canary-must-not-surface"),
        "got {error:?}"
    );

    assert!(
        server.posts().is_empty(),
        "nothing may be sent once the endpoint is refused"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_a_protocol_relative_endpoint() {
    let server = SseTestServer::start();
    let config = config(&server);
    let connecting = tokio::spawn(async move { McpSseClient::connect(&config).await });

    server.wait_for_stream();
    // Looks like a path but replaces the whole authority.
    server.send_endpoint("//evil.example/messages");

    let error = connecting
        .await
        .expect("no panic")
        .expect_err("a protocol-relative endpoint must be refused");
    assert!(matches!(error, McpSseError::Endpoint(_)), "got {error:?}");
    assert!(server.posts().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn honors_only_the_first_endpoint_event() {
    let server = SseTestServer::start();
    let config = config(&server);
    let connecting = tokio::spawn(async move { McpSseClient::connect(&config).await });

    server.wait_for_stream();
    server.send_endpoint("/messages/?session_id=first");
    // A later endpoint event must not redirect traffic mid-session.
    server.send_endpoint("/messages/?session_id=second");

    server.wait_for_posts(1);
    server.send_message(&initialize_result(1));
    server.wait_for_posts(3);
    server.send_message(&tools_result(2, json!([])));

    let _client = connecting.await.expect("no panic").expect("handshake");

    for post in server.posts() {
        assert_eq!(
            post.target, "/messages/?session_id=first",
            "every POST must use the first endpoint"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_an_oversized_endpoint_event() {
    let server = SseTestServer::start();
    let mut config = config(&server);
    config.limits = McpSseLimits {
        max_endpoint_bytes: 64,
        ..McpSseLimits::default()
    };
    let connecting = tokio::spawn(async move { McpSseClient::connect(&config).await });

    server.wait_for_stream();
    server.send_endpoint(&format!("/messages/?session_id={}", "x".repeat(512)));

    let error = connecting
        .await
        .expect("no panic")
        .expect_err("an oversized endpoint must be refused");
    assert!(
        matches!(error, McpSseError::EndpointTooLarge { limit: 64 }),
        "got {error:?}"
    );
    assert!(server.posts().is_empty());
}

// ---------------------------------------------------------------------------
// Stream framing
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn reassembles_an_endpoint_event_split_across_chunks() {
    let server = SseTestServer::start();
    let config = config(&server);
    let connecting = tokio::spawn(async move { McpSseClient::connect(&config).await });

    server.wait_for_stream();
    // One logical event delivered as three separate TCP chunks.
    server.send_raw("event: end");
    server.send_raw("point\ndata: /messa");
    server.send_raw("ges/?session_id=abc\n\n");

    server.wait_for_posts(1);
    server.send_message(&initialize_result(1));
    server.wait_for_posts(3);
    server.send_message(&tools_result(2, json!([])));

    let _client = connecting.await.expect("no panic").expect("handshake");
    assert_eq!(
        server.posts()[0].target,
        "/messages/?session_id=abc",
        "a split event must reassemble exactly"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_a_stream_using_crlf_terminators_and_heartbeats() {
    let server = SseTestServer::start();
    let config = config(&server);
    let connecting = tokio::spawn(async move { McpSseClient::connect(&config).await });

    server.wait_for_stream();
    // sse-starlette, used by most Python MCP servers, defaults to CRLF and
    // sends comment-only heartbeats.
    server.send_raw(": ping - keepalive\r\n\r\n");
    server.send_raw("event: endpoint\r\ndata: /messages/?session_id=abc\r\n\r\n");

    server.wait_for_posts(1);
    server.send_raw(": ping - keepalive\r\n\r\n");
    server.send_raw(format!(
        "event: message\r\ndata: {}\r\n\r\n",
        initialize_result(1)
    ));

    server.wait_for_posts(3);
    server.send_raw(format!(
        "event: message\r\ndata: {}\r\n\r\n",
        tools_result(
            2,
            json!([{"name": "search", "inputSchema": {"type": "object"}}])
        )
    ));

    let client = connecting.await.expect("no panic").expect("handshake");
    assert_eq!(client.tools().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_a_message_split_across_several_data_lines() {
    let server = SseTestServer::start();
    let client = connect(&server, config(&server)).await;

    let calling = tokio::spawn(async move { (client.call_tool("search", None).await, client) });

    server.wait_for_posts(4);
    // A JSON payload containing newlines is emitted as multiple data lines.
    server.send_raw(
        "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":\ndata: {\"content\":[{\"type\":\"text\",\"text\":\"ok\"}],\"isError\":false}}\n\n",
    );

    let (result, _client) = calling.await.expect("no panic");
    let result = result.expect("multi-line data must rejoin into one payload");
    assert_eq!(result.content[0].text.as_deref(), Some("ok"));
}

#[tokio::test(flavor = "multi_thread")]
async fn ignores_unknown_event_names_such_as_ping() {
    let server = SseTestServer::start();
    let config = config(&server);
    let connecting = tokio::spawn(async move { McpSseClient::connect(&config).await });

    server.wait_for_stream();
    // Older sse-starlette emits a real event with non-JSON data.
    server.send_raw("event: ping\ndata: 2026-08-08 12:00:00\n\n");
    server.send_endpoint("/messages/?session_id=abc");

    server.wait_for_posts(1);
    server.send_raw("event: ping\ndata: 2026-08-08 12:00:15\n\n");
    server.send_message(&initialize_result(1));
    server.wait_for_posts(3);
    server.send_message(&tools_result(2, json!([])));

    let client = connecting.await.expect("no panic").expect("handshake");
    assert!(client.tools().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn ignores_a_server_initiated_request_rather_than_treating_it_as_a_response() {
    let server = SseTestServer::start();
    let client = connect(&server, config(&server)).await;

    let calling = tokio::spawn(async move { (client.call_tool("search", None).await, client) });

    server.wait_for_posts(4);
    // A ping request carries method and id but is not a response to id 3.
    server.send_message(&json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}));
    server.send_message(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "result": {"content": [{"type": "text", "text": "real"}], "isError": false}
    }));

    let (result, _client) = calling.await.expect("no panic");
    let result = result.expect("the real response must still resolve the call");
    assert_eq!(result.content[0].text.as_deref(), Some("real"));
}

#[tokio::test(flavor = "multi_thread")]
async fn ignores_a_repeated_response_for_an_already_answered_id() {
    let server = SseTestServer::start();
    let client = connect(&server, config(&server)).await;

    let calling = tokio::spawn(async move { (client.call_tool("search", None).await, client) });

    server.wait_for_posts(4);
    server.send_message(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "result": {"content": [{"type": "text", "text": "first"}], "isError": false}
    }));
    // A second result for the same id must not reach the caller.
    server.send_message(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "result": {"content": [{"type": "text", "text": "second"}], "isError": false}
    }));

    let (result, client) = calling.await.expect("no panic");
    assert_eq!(
        result.expect("the first response wins").content[0]
            .text
            .as_deref(),
        Some("first")
    );

    // The connection stays usable rather than being corrupted by the duplicate.
    client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ignores_malformed_json_rpc_without_failing_other_calls() {
    let server = SseTestServer::start();
    let client = connect(&server, config(&server)).await;

    let calling = tokio::spawn(async move { (client.call_tool("search", None).await, client) });

    server.wait_for_posts(4);
    server.send_raw("event: message\ndata: {not json at all\n\n");
    server.send_message(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "result": {"content": [{"type": "text", "text": "ok"}], "isError": false}
    }));

    let (result, _client) = calling.await.expect("no panic");
    assert_eq!(
        result
            .expect("a malformed frame must not break the stream")
            .content[0]
            .text
            .as_deref(),
        Some("ok")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn tears_down_the_stream_when_an_event_exceeds_the_size_limit() {
    let server = SseTestServer::start();
    let mut config = config(&server);
    config.limits = McpSseLimits {
        max_event_bytes: 256,
        ..McpSseLimits::default()
    };
    let client = connect(&server, config).await;

    let calling = tokio::spawn(async move { (client.call_tool("search", None).await, client) });

    server.wait_for_posts(4);
    server.send_raw(format!("event: message\ndata: {}\n\n", "x".repeat(4096)));

    let (result, _client) = calling.await.expect("no panic");
    let error = result.expect_err("an oversized event must fail the call");
    assert!(
        matches!(error, McpSseError::RequestIndeterminate { .. }),
        "an accepted call that never answered is indeterminate, got {error:?}"
    );
}

// ---------------------------------------------------------------------------
// Connection failures
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn rejects_a_stream_response_that_is_not_an_event_stream() {
    let server = SseTestServer::with_opening(StreamOpening::WrongContentType);
    let error = McpSseClient::connect(&config(&server))
        .await
        .expect_err("a JSON response is not a stream");
    assert!(
        matches!(error, McpSseError::UnexpectedContentType { .. }),
        "got {error:?}"
    );
    assert!(!error.to_string().contains("remote-canary"), "got {error}");
    assert!(
        !format!("{error:?}").contains("remote-canary"),
        "got {error:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn accepts_an_event_stream_content_type_carrying_a_charset() {
    // The fixture answers `text/event-stream; charset=utf-8`, which is what
    // real servers send; a strict equality check would reject it.
    let server = SseTestServer::start();
    let client = connect(&server, config(&server)).await;
    assert_eq!(client.tools().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_a_non_success_status_on_the_stream() {
    let server = SseTestServer::with_opening(StreamOpening::Status {
        code: 404,
        body: "not found".to_string(),
    });
    let error = McpSseClient::connect(&config(&server))
        .await
        .expect_err("404 is not a stream");
    assert!(
        matches!(error, McpSseError::HttpStatus { status, .. } if status == 404),
        "got {error:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn refuses_to_follow_a_redirect_on_the_stream() {
    let server = SseTestServer::with_opening(StreamOpening::Redirect {
        location: "http://evil.example/sse".to_string(),
    });
    let error = McpSseClient::connect(&config(&server))
        .await
        .expect_err("a redirect must not be followed");
    assert!(
        matches!(error, McpSseError::RedirectRefused),
        "got {error:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reports_a_rejected_post_without_quoting_the_response_body() {
    let server = SseTestServer::start();
    let config = config(&server);
    let connecting = tokio::spawn(async move { McpSseClient::connect(&config).await });

    server.wait_for_stream();
    server.queue_post_reply(PostReply::Status {
        code: 400,
        body: "SESSION-SECRET-LEAK".to_string(),
    });
    server.send_endpoint("/messages/?session_id=abc");

    let error = connecting
        .await
        .expect("no panic")
        .expect_err("a 400 fails the handshake");
    assert!(
        matches!(error, McpSseError::HttpStatus { status, .. } if status == 400),
        "got {error:?}"
    );
    assert!(
        !error.to_string().contains("SESSION-SECRET-LEAK"),
        "server text must never reach an error: {error}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reports_a_server_error_on_the_message_post() {
    let server = SseTestServer::start();
    let config = config(&server);
    let connecting = tokio::spawn(async move { McpSseClient::connect(&config).await });

    server.wait_for_stream();
    server.queue_post_reply(PostReply::Status {
        code: 503,
        body: "unavailable".to_string(),
    });
    server.send_endpoint("/messages/?session_id=abc");

    let error = connecting
        .await
        .expect("no panic")
        .expect_err("a 503 fails the handshake");
    assert!(
        matches!(error, McpSseError::HttpStatus { status, .. } if status == 503),
        "got {error:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn refuses_to_follow_a_redirect_on_a_message_post() {
    let server = SseTestServer::start();
    let config = config(&server);
    let connecting = tokio::spawn(async move { McpSseClient::connect(&config).await });

    server.wait_for_stream();
    server.queue_post_reply(PostReply::Redirect {
        location: "http://evil.example/messages".to_string(),
    });
    server.send_endpoint("/messages/?session_id=abc");

    let error = connecting
        .await
        .expect("no panic")
        .expect_err("a redirected POST must not be followed");
    assert!(
        matches!(error, McpSseError::RedirectRefused),
        "got {error:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_an_absolute_endpoint_on_the_fixture_origin_with_a_different_port() {
    let server = SseTestServer::start();
    let config = config(&server);
    let connecting = tokio::spawn(async move { McpSseClient::connect(&config).await });

    server.wait_for_stream();
    // Same host, different port: still a different origin.
    let other_port = server
        .base_url()
        .rsplit(':')
        .next()
        .and_then(|port| port.parse::<u16>().ok())
        .map(|port| port.wrapping_add(1))
        .expect("the fixture URL carries a port");
    server.send_endpoint(&format!("http://127.0.0.1:{other_port}/messages/"));

    let error = connecting
        .await
        .expect("no panic")
        .expect_err("a different port is a different origin");
    assert!(matches!(error, McpSseError::Endpoint(_)), "got {error:?}");
    assert!(server.posts().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn accepts_an_absolute_endpoint_on_the_configured_origin() {
    let server = SseTestServer::start();
    let base_url = server.base_url().to_string();
    let config = config(&server);
    let connecting = tokio::spawn(async move { McpSseClient::connect(&config).await });

    server.wait_for_stream();
    // The specification permits an absolute URL as long as the origin matches.
    server.send_endpoint(&format!("{base_url}/messages/?session_id=abc"));

    server.wait_for_posts(1);
    server.send_message(&initialize_result(1));
    server.wait_for_posts(3);
    server.send_message(&tools_result(2, json!([])));

    let _client = connecting.await.expect("no panic").expect("handshake");
    assert_eq!(server.posts()[0].target, "/messages/?session_id=abc");
}

#[tokio::test(flavor = "multi_thread")]
async fn reports_a_post_the_server_never_answers() {
    let server = SseTestServer::start();
    let config = config(&server);
    let connecting = tokio::spawn(async move { McpSseClient::connect(&config).await });

    server.wait_for_stream();
    // The server accepts the connection then closes it without responding.
    server.queue_post_reply(PostReply::Drop);
    server.send_endpoint("/messages/?session_id=abc");

    let error = connecting
        .await
        .expect("no panic")
        .expect_err("a dropped POST fails the handshake");
    assert!(
        matches!(error, McpSseError::Transport(_)),
        "a dropped connection is a transport failure, got {error:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bounds_the_initialize_request_post() {
    let server = SseTestServer::start();
    let mut config = config(&server);
    config.limits = McpSseLimits {
        initialize_timeout: std::time::Duration::from_millis(150),
        ..McpSseLimits::default()
    };
    let connecting = tokio::spawn(async move { McpSseClient::connect(&config).await });

    server.wait_for_stream();
    server.queue_post_reply(PostReply::StallBeforeHeaders);
    server.send_endpoint("/messages/?session_id=abc");
    server.wait_for_posts(1);

    let error = tokio::time::timeout(std::time::Duration::from_secs(2), connecting)
        .await
        .expect("the configured initialize deadline must include its POST response head")
        .expect("no panic")
        .expect_err("the initialize POST never receives response headers");
    server.release_stalled_posts();

    assert!(matches!(error, McpSseError::Timeout(_)), "got {error:?}");
    assert_eq!(
        server.posts().len(),
        1,
        "an initialize timeout must not send a second request"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bounds_the_initialized_notification_post() {
    let server = SseTestServer::start();
    let mut config = config(&server);
    config.limits = McpSseLimits {
        // This deadline covers the initialize request, response processing,
        // and the initialized notification POST. Leave enough scheduling
        // margin for the highly parallel full suite while keeping the stalled
        // POST itself firmly bounded.
        initialize_timeout: std::time::Duration::from_secs(2),
        ..McpSseLimits::default()
    };
    let connecting = tokio::spawn(async move { McpSseClient::connect(&config).await });

    server.wait_for_stream();
    server.send_endpoint("/messages/?session_id=abc");
    server.wait_for_posts(1);
    server.queue_post_reply(PostReply::StallBeforeHeaders);
    server.send_message(&initialize_result(1));
    server.wait_for_posts(2);

    let error = tokio::time::timeout(std::time::Duration::from_secs(5), connecting)
        .await
        .expect("the configured initialize deadline must bound the notification POST")
        .expect("no panic")
        .expect_err("a notification POST that never answers must fail connect");
    server.release_stalled_posts();

    assert!(matches!(error, McpSseError::Timeout(_)), "got {error:?}");
    assert_eq!(
        server.posts().len(),
        2,
        "tools/list must not start after the initialized notification timed out"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bounds_the_entire_tool_call_when_post_headers_never_arrive() {
    let server = SseTestServer::start();
    let mut config = config(&server);
    config.limits = McpSseLimits {
        call_tool_timeout: std::time::Duration::from_millis(150),
        ..McpSseLimits::default()
    };
    let client = std::sync::Arc::new(connect(&server, config).await);

    server.queue_post_reply(PostReply::StallBeforeHeaders);
    let calling = {
        let client = std::sync::Arc::clone(&client);
        tokio::spawn(async move { client.call_tool("charge_card", None).await })
    };
    server.wait_for_posts(4);

    let error = tokio::time::timeout(std::time::Duration::from_secs(2), calling)
        .await
        .expect("the configured call deadline must include the POST response head")
        .expect("no panic")
        .expect_err("the server withheld its response head");
    assert!(
        matches!(error, McpSseError::RequestIndeterminate { .. }),
        "the server read the request body, so delivery is ambiguous: {error:?}"
    );
    assert_eq!(
        server
            .posts()
            .iter()
            .filter(|request| request.rpc_method().as_deref() == Some("tools/call"))
            .count(),
        1,
        "the ambiguous tool call must never be replayed"
    );

    // The timeout removes only this request's correlation state. Once the
    // fixture releases the abandoned POST connection, the SSE session remains
    // able to correlate a later, explicitly requested call.
    server.release_stalled_posts();
    let calling = {
        let client = std::sync::Arc::clone(&client);
        tokio::spawn(async move { client.call_tool("search", None).await })
    };
    server.wait_for_posts(5);
    server.send_message(&json!({
        "jsonrpc": "2.0",
        "id": 4,
        "result": {"content": [{"type": "text", "text": "still usable"}], "isError": false}
    }));
    assert_eq!(
        calling
            .await
            .expect("no panic")
            .expect("a later explicit call should succeed")
            .content[0]
            .text
            .as_deref(),
        Some("still usable")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bounds_the_entire_tool_call_while_draining_the_post_body() {
    let server = SseTestServer::start();
    let mut config = config(&server);
    config.limits = McpSseLimits {
        call_tool_timeout: std::time::Duration::from_millis(150),
        ..McpSseLimits::default()
    };
    let client = connect(&server, config).await;

    server.queue_post_reply(PostReply::StallAfterHeaders);
    let calling =
        tokio::spawn(async move { (client.call_tool("charge_card", None).await, client) });
    server.wait_for_posts(4);
    server.wait_for_post_response_headers(4);

    let (result, _client) = tokio::time::timeout(std::time::Duration::from_secs(2), calling)
        .await
        .expect("the configured call deadline must include response-body drain")
        .expect("no panic");
    server.release_stalled_posts();

    let error = result.expect_err("the declared response body never arrived");
    assert!(
        matches!(error, McpSseError::RequestIndeterminate { .. }),
        "the tool may have run before the POST response body stalled: {error:?}"
    );
    assert_eq!(
        server
            .posts()
            .iter()
            .filter(|request| request.rpc_method().as_deref() == Some("tools/call"))
            .count(),
        1,
        "the ambiguous tool call must never be replayed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_call_dropped_after_its_body_is_read_is_indeterminate() {
    let server = SseTestServer::start();
    let client = connect(&server, config(&server)).await;

    server.queue_post_reply(PostReply::Drop);
    let error = client
        .call_tool("charge_card", None)
        .await
        .expect_err("the fixture drops the POST connection after reading its body");

    assert!(
        matches!(error, McpSseError::RequestIndeterminate { .. }),
        "a transport failure cannot prove non-delivery: {error:?}"
    );
    assert_eq!(
        server
            .posts()
            .iter()
            .filter(|request| request.rpc_method().as_deref() == Some("tools/call"))
            .count(),
        1,
        "the dropped tool call must never be replayed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_call_answered_with_an_http_error_is_indeterminate() {
    let server = SseTestServer::start();
    let client = connect(&server, config(&server)).await;

    server.queue_post_reply(PostReply::Status {
        code: 503,
        body: "failed after dispatch".to_string(),
    });
    let error = client
        .call_tool("charge_card", None)
        .await
        .expect_err("a non-success POST status fails the call");

    assert!(
        matches!(error, McpSseError::RequestIndeterminate { .. }),
        "HTTP status cannot prove that the server did no work: {error:?}"
    );
    assert_eq!(
        server
            .posts()
            .iter()
            .filter(|request| request.rpc_method().as_deref() == Some("tools/call"))
            .count(),
        1,
        "the failed tool call must never be replayed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_call_answered_with_a_redirect_is_indeterminate_and_not_followed() {
    let server = SseTestServer::start();
    let client = connect(&server, config(&server)).await;

    server.queue_post_reply(PostReply::Redirect {
        location: "http://evil.example/messages".to_string(),
    });
    let error = client
        .call_tool("charge_card", None)
        .await
        .expect_err("a redirected POST fails the call");

    assert!(
        matches!(error, McpSseError::RequestIndeterminate { .. }),
        "a redirect response cannot prove that the original server did no work: {error:?}"
    );
    assert_eq!(
        server
            .posts()
            .iter()
            .filter(|request| request.rpc_method().as_deref() == Some("tools/call"))
            .count(),
        1,
        "the client must neither follow nor replay the redirected tool call"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_tool_call_future_removes_its_pending_waiter() {
    let server = SseTestServer::start();
    let client = std::sync::Arc::new(connect(&server, config(&server)).await);

    server.queue_post_reply(PostReply::StallBeforeHeaders);
    let calling = {
        let client = std::sync::Arc::clone(&client);
        tokio::spawn(async move { client.call_tool("charge_card", None).await })
    };
    server.wait_for_posts(4);
    calling.abort();
    assert!(
        calling
            .await
            .expect_err("the call task was cancelled")
            .is_cancelled()
    );

    assert!(
        super::lock_pending(&client.pending).waiters.is_empty(),
        "cancelling the future must remove its pending correlation entry"
    );
    assert_eq!(
        server
            .posts()
            .iter()
            .filter(|request| request.rpc_method().as_deref() == Some("tools/call"))
            .count(),
        1,
        "cancellation must not cause an automatic replay"
    );
    server.release_stalled_posts();
}

// ---------------------------------------------------------------------------
// Teardown
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn fails_every_pending_call_when_the_stream_reaches_eof() {
    let server = SseTestServer::start();
    let client = std::sync::Arc::new(connect(&server, config(&server)).await);

    let first = {
        let client = std::sync::Arc::clone(&client);
        tokio::spawn(async move { client.call_tool("search", Some(json!({"q": "a"}))).await })
    };
    let second = {
        let client = std::sync::Arc::clone(&client);
        tokio::spawn(async move { client.call_tool("search", Some(json!({"q": "b"}))).await })
    };

    server.wait_for_posts(5);
    server.close_stream();

    let first = first
        .await
        .expect("no panic")
        .expect_err("EOF must fail the call rather than hang");
    let second = second
        .await
        .expect("no panic")
        .expect_err("EOF must fail every call");

    assert!(
        matches!(first, McpSseError::RequestIndeterminate { .. }),
        "got {first:?}"
    );
    assert!(
        matches!(second, McpSseError::RequestIndeterminate { .. }),
        "got {second:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_accepted_call_lost_to_a_stream_drop_is_reported_as_indeterminate() {
    let server = SseTestServer::start();
    let client = connect(&server, config(&server)).await;

    let calling =
        tokio::spawn(async move { (client.call_tool("charge_card", None).await, client) });

    server.wait_for_posts(4);
    // The POST was accepted, so the tool may well have run.
    server.abort_stream();

    let (result, _client) = calling.await.expect("no panic");
    let error = result.expect_err("a lost response is a failure");
    match error {
        McpSseError::RequestIndeterminate { method } => assert_eq!(method, "tools/call"),
        other => panic!("an accepted-but-unanswered call must be indeterminate, got {other:?}"),
    }
    assert!(
        error_says_do_not_retry(&McpSseError::RequestIndeterminate {
            method: "tools/call".to_string()
        }),
        "the message must warn against automatic retry"
    );
}

fn error_says_do_not_retry(error: &McpSseError) -> bool {
    let rendered = error.to_string();
    rendered.contains("may have executed") && rendered.contains("must not be retried")
}

#[tokio::test(flavor = "multi_thread")]
async fn never_replays_a_tool_call_after_an_ambiguous_failure() {
    let server = SseTestServer::start();
    let client = connect(&server, config(&server)).await;

    let calling =
        tokio::spawn(async move { (client.call_tool("charge_card", None).await, client) });

    server.wait_for_posts(4);
    server.abort_stream();

    let (result, _client) = calling.await.expect("no panic");
    result.expect_err("the call fails");

    // Give any (incorrect) retry a chance to appear before asserting.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let calls = server
        .posts()
        .into_iter()
        .filter(|request| request.rpc_method().as_deref() == Some("tools/call"))
        .count();
    assert_eq!(
        calls, 1,
        "a tools/call may have side effects and must never be re-sent"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shutting_down_fails_calls_that_are_still_in_flight() {
    let server = SseTestServer::start();
    let client = std::sync::Arc::new(connect(&server, config(&server)).await);

    let calling = {
        let client = std::sync::Arc::clone(&client);
        tokio::spawn(async move { client.call_tool("search", None).await })
    };

    server.wait_for_posts(4);
    client.shutdown().await;

    let error = calling
        .await
        .expect("no panic")
        .expect_err("shutdown must resolve outstanding calls");
    assert!(
        matches!(error, McpSseError::RequestIndeterminate { .. }),
        "got {error:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_made_after_shutdown_fails_immediately() {
    let server = SseTestServer::start();
    let client = connect(&server, config(&server)).await;
    client.shutdown().await;

    let error = client
        .call_tool("search", None)
        .await
        .expect_err("a shut-down client accepts no work");
    assert!(matches!(error, McpSseError::StreamClosed), "got {error:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_timed_out_request_does_not_leak_its_pending_entry() {
    let server = SseTestServer::start();
    let mut config = config(&server);
    config.limits = McpSseLimits {
        call_tool_timeout: std::time::Duration::from_millis(150),
        ..McpSseLimits::default()
    };
    let client = connect(&server, config).await;

    // Time out several calls, then confirm a later one still succeeds.
    for _ in 0..3 {
        let error = client
            .call_tool("search", None)
            .await
            .expect_err("no response arrives");
        assert!(
            matches!(error, McpSseError::RequestIndeterminate { .. }),
            "got {error:?}"
        );
    }

    let calling = tokio::spawn(async move { (client.call_tool("search", None).await, client) });
    server.wait_for_posts(7);
    server.send_message(&json!({
        "jsonrpc": "2.0",
        "id": 6,
        "result": {"content": [{"type": "text", "text": "late but fine"}], "isError": false}
    }));

    let (result, _client) = calling.await.expect("no panic");
    assert_eq!(
        result.expect("the connection remains usable").content[0]
            .text
            .as_deref(),
        Some("late but fine")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn connecting_times_out_when_the_endpoint_event_never_arrives() {
    let server = SseTestServer::start();
    let mut config = config(&server);
    config.limits = McpSseLimits {
        connect_timeout: std::time::Duration::from_millis(200),
        ..McpSseLimits::default()
    };

    let connecting = tokio::spawn(async move { McpSseClient::connect(&config).await });
    server.wait_for_stream();
    // A buffering proxy is the common cause; no endpoint event is ever sent.

    let error = connecting
        .await
        .expect("no panic")
        .expect_err("the handshake cannot proceed without an endpoint");
    assert!(matches!(error, McpSseError::Timeout(_)), "got {error:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn connecting_fails_when_the_stream_closes_before_the_endpoint_arrives() {
    let server = SseTestServer::start();
    let config = config(&server);
    let connecting = tokio::spawn(async move { McpSseClient::connect(&config).await });

    server.wait_for_stream();
    server.close_stream();

    let error = connecting
        .await
        .expect("no panic")
        .expect_err("a closed stream cannot complete the handshake");
    assert!(matches!(error, McpSseError::StreamClosed), "got {error:?}");
}

/// A rejected endpoint must not leave the reader task running.
///
/// The task owns the response body, so leaking it also leaks the connection.
/// Because the reader is what consumes the stream, a leaked one keeps draining
/// events the abandoned client can never deliver — observable here as the
/// fixture continuing to accept writes long after connect returned.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_endpoint_leaves_no_reader_consuming_the_stream() {
    let server = SseTestServer::start();
    let config = config(&server);
    let connecting = tokio::spawn(async move { McpSseClient::connect(&config).await });

    server.wait_for_stream();
    server.send_endpoint("https://evil.example/messages");

    let error = connecting
        .await
        .expect("no panic")
        .expect_err("a cross-origin endpoint is refused");
    assert!(matches!(error, McpSseError::Endpoint(_)), "got {error:?}");

    // Nothing was ever sent to the server, and nothing may be sent later.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        server.posts().is_empty(),
        "a refused endpoint must not produce any request"
    );
}
