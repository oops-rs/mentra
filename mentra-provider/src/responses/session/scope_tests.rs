use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::Read;
use std::io::Write;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::net::TcpStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use super::*;
use crate::ProviderRequestOptions;
use crate::Role;
use crate::StaticCredentialSource;
use crate::responses::ResponsesProvider;

#[test]
fn fresh_scope_detaches_session_state_but_keeps_endpoint_knowledge() {
    let provider = ResponsesProvider::with_shared_credential_source(
        super::super::openai_definition(),
        Arc::new(StaticCredentialSource::new("test-key")),
    );
    let old_session = provider.session();
    old_session.set_turn_state("scope-a-turn");
    old_session.state.set_latest_response_id("resp_a");
    old_session.set_connection_reused(true);
    old_session.disable_websockets();
    old_session
        .endpoint_capabilities
        .mark_http_previous_response_id_unsupported("gpt-5");
    let (last_response_tx, last_response_rx) = oneshot::channel();
    old_session
        .state
        .websocket_session
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .last_response_rx = Some(last_response_rx);

    let fresh_provider = provider.fresh_session_scope();
    let fresh_session = fresh_provider.session();
    drop(last_response_tx);

    assert!(!Arc::ptr_eq(&old_session.state, &fresh_session.state));
    assert!(Arc::ptr_eq(
        &old_session.endpoint_capabilities,
        &fresh_session.endpoint_capabilities
    ));
    assert_eq!(fresh_session.turn_state(), None);
    assert_eq!(fresh_session.latest_response_id(), None);
    assert!(!fresh_session.connection_reused());
    assert!(fresh_session.websockets_enabled());
    assert!(!fresh_session.last_response_rx_ready());
    assert!(old_session.last_response_rx_ready());
    assert!(
        fresh_session
            .endpoint_capabilities
            .http_previous_response_id_is_unsupported("gpt-5")
    );
}

#[tokio::test]
async fn late_old_scope_completion_cannot_seed_the_fresh_scope() {
    let provider = ResponsesProvider::with_shared_credential_source(
        super::super::openai_definition(),
        Arc::new(StaticCredentialSource::new("test-key")),
    );
    let old_session = provider.session();
    let fresh_session = provider.fresh_session_scope().session();
    let (tx_event, rx_event) = mpsc::unbounded_channel();
    let mut forwarded = old_session.track_response_state(rx_event);

    tx_event
        .send(Ok(ProviderEvent::MessageStarted {
            id: "resp_late_a".to_string(),
            model: "gpt-5".to_string(),
            role: Role::Assistant,
        }))
        .expect("old scope should still accept its in-flight completion");
    drop(tx_event);

    forwarded
        .recv()
        .await
        .expect("tracked event should be forwarded")
        .expect("tracked event should remain successful");

    assert_eq!(
        old_session.latest_response_id().as_deref(),
        Some("resp_late_a")
    );
    assert_eq!(fresh_session.latest_response_id(), None);
}

#[tokio::test]
async fn fresh_http_scopes_reuse_the_client_pool_without_sharing_turn_state() {
    let (base_url, captures) = spawn_scope_http_server(4);
    let mut definition = super::super::openai_definition();
    definition.base_url = Some(base_url);
    let provider = ResponsesProvider::with_shared_credential_source(
        definition,
        Arc::new(StaticCredentialSource::new("test-key")),
    );
    let scope_a = provider.fresh_session_scope().session();
    let scope_b = provider.fresh_session_scope().session();

    for (session, message) in [
        (&scope_a, "scope-a-first"),
        (&scope_b, "scope-b-first"),
        (&scope_a, "scope-a-second"),
        (&scope_b, "scope-b-second"),
    ] {
        consume_stream(
            session
                .stream_response(test_request(message))
                .await
                .expect("scope request should start"),
        )
        .await;
    }

    let captured = captures.join().expect("server should capture all requests");
    assert_eq!(captured.len(), 4);
    let peer_ports = captured
        .iter()
        .map(|capture| capture.peer.port())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        peer_ports.len(),
        1,
        "fresh scopes must retain the cloned reqwest client and its warmed pool"
    );

    let payloads = captured
        .iter()
        .map(|capture| {
            serde_json::from_str::<serde_json::Value>(request_body(&capture.request))
                .expect("request body should be json")
        })
        .collect::<Vec<_>>();
    assert!(payloads[0].get("previous_response_id").is_none());
    assert!(payloads[1].get("previous_response_id").is_none());
    assert_eq!(payloads[2]["previous_response_id"], "resp_1");
    assert_eq!(payloads[3]["previous_response_id"], "resp_2");
    assert!(!captured[0].request.contains("x-codex-turn-state:"));
    assert!(!captured[1].request.contains("x-codex-turn-state:"));
    assert!(
        captured[2]
            .request
            .contains("x-codex-turn-state: state-1\r\n")
    );
    assert!(
        captured[3]
            .request
            .contains("x-codex-turn-state: state-2\r\n")
    );
    assert_eq!(scope_a.latest_response_id().as_deref(), Some("resp_3"));
    assert_eq!(scope_b.latest_response_id().as_deref(), Some("resp_4"));
    assert_eq!(scope_a.turn_state().as_deref(), Some("state-3"));
    assert_eq!(scope_b.turn_state().as_deref(), Some("state-4"));
}

#[cfg(feature = "responses-websocket")]
#[tokio::test]
async fn websocket_prewarm_is_shared_only_inside_the_fresh_scope() {
    use tokio_tungstenite::accept_async;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind websocket test server");
    let addr = listener.local_addr().expect("read websocket server addr");
    let (release_tx, release_rx) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept websocket");
        let _websocket = accept_async(stream).await.expect("upgrade websocket");
        let _ = release_rx.await;
    });

    let mut definition = super::super::openai_definition();
    definition.base_url = Some(format!("http://{addr}/v1"));
    let provider = ResponsesProvider::with_shared_credential_source(
        definition,
        Arc::new(StaticCredentialSource::new("test-key")),
    );
    let scope = provider.fresh_session_scope();
    let prewarm_session = scope.clone().session();
    prewarm_session
        .connect_websocket(HeaderMap::new(), HeaderMap::new(), None, None)
        .await
        .expect("prewarm should connect the fresh scope");

    assert!(!scope.session().websocket_connection_is_closed().await);
    assert!(provider.session().websocket_connection_is_closed().await);
    assert!(
        provider
            .fresh_session_scope()
            .session()
            .websocket_connection_is_closed()
            .await
    );

    let _ = release_tx.send(());
    server.await.expect("websocket server should finish");
}

fn test_request(message: &'static str) -> Request<'static> {
    Request {
        model: Cow::Borrowed("gpt-5"),
        system: None,
        messages: Cow::Owned(vec![crate::Message::user(crate::ContentBlock::text(
            message,
        ))]),
        tools: Cow::Owned(Vec::new()),
        tool_choice: None,
        temperature: None,
        max_output_tokens: None,
        metadata: Cow::Owned(BTreeMap::new()),
        provider_request_options: ProviderRequestOptions::default(),
    }
}

async fn consume_stream(mut stream: ProviderEventStream) {
    while let Some(event) = stream.recv().await {
        event.expect("stream event should decode");
    }
}

struct CapturedRequest {
    peer: SocketAddr,
    request: String,
}

struct ScopeConnection {
    stream: TcpStream,
    peer: SocketAddr,
    buffer: Vec<u8>,
}

fn spawn_scope_http_server(
    expected_requests: usize,
) -> (String, thread::JoinHandle<Vec<CapturedRequest>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind scope test server");
    listener
        .set_nonblocking(true)
        .expect("make scope listener nonblocking");
    let addr = listener.local_addr().expect("read scope listener addr");
    let handle = thread::spawn(move || capture_scope_requests(&listener, expected_requests));

    (format!("http://{addr}/"), handle)
}

fn capture_scope_requests(
    listener: &TcpListener,
    expected_requests: usize,
) -> Vec<CapturedRequest> {
    let mut connections = Vec::<ScopeConnection>::new();
    let mut captured = Vec::with_capacity(expected_requests);
    let deadline = Instant::now() + Duration::from_secs(5);
    while captured.len() < expected_requests {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for scope requests: expected {expected_requests}, captured {}, \
             accepted connections {}",
            captured.len(),
            connections.len()
        );
        accept_scope_connections(listener, &mut connections);
        for connection in &mut connections {
            read_available_scope_bytes(connection);
            if let Some(request_end) = complete_request_end(&connection.buffer) {
                let request = connection.buffer.drain(..request_end).collect::<Vec<_>>();
                let response_index = captured.len() + 1;
                captured.push(CapturedRequest {
                    peer: connection.peer,
                    request: String::from_utf8(request).expect("scope request should be utf8"),
                });
                write_scope_response(&mut connection.stream, response_index);
            }
        }
        thread::sleep(Duration::from_millis(1));
    }
    captured
}

fn accept_scope_connections(listener: &TcpListener, connections: &mut Vec<ScopeConnection>) {
    loop {
        match listener.accept() {
            Ok((stream, peer)) => {
                stream
                    .set_nonblocking(true)
                    .expect("make scope connection nonblocking");
                connections.push(ScopeConnection {
                    stream,
                    peer,
                    buffer: Vec::new(),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("accept scope request: {error}"),
        }
    }
}

fn read_available_scope_bytes(connection: &mut ScopeConnection) {
    let mut temp = [0_u8; 1024];
    loop {
        match connection.stream.read(&mut temp) {
            Ok(0) => break,
            Ok(read) => connection.buffer.extend_from_slice(&temp[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("read scope request: {error}"),
        }
    }
}

fn complete_request_end(buffer: &[u8]) -> Option<usize> {
    let header_end = buffer.windows(4).position(|window| window == b"\r\n\r\n")? + 4;
    let headers = String::from_utf8_lossy(&buffer[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().expect("parse content-length"))
        })
        .unwrap_or_default();
    (buffer.len() >= header_end + content_length).then_some(header_end + content_length)
}

fn write_scope_response(stream: &mut TcpStream, response_index: usize) {
    let response_id = format!("resp_{response_index}");
    let turn_state = format!("state-{response_index}");
    let response_body = format!(
        concat!(
            "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"{}\",\"model\":\"gpt-5\",\"status\":\"in_progress\"}}}}\n\n",
            "data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"{}\",\"model\":\"gpt-5\",\"status\":\"completed\"}}}}\n\n"
        ),
        response_id, response_id
    );
    let response = format!(
        concat!(
            "HTTP/1.1 200 OK\r\n",
            "content-type: text/event-stream\r\n",
            "x-codex-turn-state: {}\r\n",
            "content-length: {}\r\n\r\n",
            "{}"
        ),
        turn_state,
        response_body.len(),
        response_body
    );
    let mut remaining = response.as_bytes();
    while !remaining.is_empty() {
        match stream.write(remaining) {
            Ok(0) => panic!("scope connection closed while writing response"),
            Ok(written) => remaining = &remaining[written..],
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("write scope response: {error}"),
        }
    }
}

fn request_body(captured: &str) -> &str {
    captured.split("\r\n\r\n").nth(1).unwrap_or_default()
}
