//! A `previous_response_id` the endpoint refuses over the websocket transport.
//!
//! The refusal arrives as an `error` frame after the upgrade, so the transport
//! reports it as an item of the event stream rather than as an error from the
//! call that opened it. These tests drive a real websocket server through
//! [`ResponsesSession::stream_response`] to pin what happens next.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use futures_util::SinkExt;
use futures_util::StreamExt;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::*;
use crate::ProviderRequestOptions;
use crate::ResponsesStateMode;
use crate::StaticCredentialSource;
use crate::responses::ResponsesProvider;

/// The body a production gateway sent for a chained id it no longer knew.
fn invalid_previous_response_id_frame() -> serde_json::Value {
    serde_json::json!({
        "type": "error",
        "status": 400,
        "error": {
            "type": "invalid_request_error",
            "message": "Invalid previous_response_id."
        }
    })
}

#[tokio::test]
async fn hybrid_websocket_resends_without_a_rejected_previous_response_id() {
    let (base_url, mut frames) = spawn_websocket_server(vec![
        Reply::Frame(invalid_previous_response_id_frame()),
        Reply::Complete("resp_fresh"),
        Reply::Complete("resp_next"),
    ])
    .await;
    let session = websocket_session(base_url);
    session.state.set_latest_response_id("resp_stale");

    let events = collect(
        session
            .stream_response(transcript_request(ResponsesStateMode::Hybrid))
            .await
            .expect("a rejected chain should be recovered, not returned"),
    )
    .await;

    assert!(
        events.iter().all(Result::is_ok),
        "the recovered stream must not carry the refusal: {events:?}"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        Ok(ProviderEvent::MessageStarted { id, .. }) if id == "resp_fresh"
    )));
    assert_eq!(session.latest_response_id().as_deref(), Some("resp_fresh"));

    let captured = drain(&mut frames);
    assert_eq!(captured.len(), 2, "exactly one resend: {captured:?}");
    assert_eq!(captured[0].frame["previous_response_id"], "resp_stale");
    assert!(captured[1].frame.get("previous_response_id").is_none());
    assert_eq!(
        captured[1].frame["input"], captured[0].frame["input"],
        "dropping the chained id must not drop any of the replayed transcript"
    );
    assert_eq!(
        captured[1].frame["input"]
            .as_array()
            .expect("input should be an array")
            .len(),
        3
    );
    let resend_connection = captured[1].connection;

    // The chain resumes from the resend's response, on the connection that
    // produced it.
    consume(
        session
            .stream_response(transcript_request(ResponsesStateMode::Hybrid))
            .await
            .expect("the recovered chain should stream"),
    )
    .await;
    let captured = drain(&mut frames);
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].frame["previous_response_id"], "resp_fresh");
    assert_eq!(captured[0].connection, resend_connection);
    assert_eq!(session.latest_response_id().as_deref(), Some("resp_next"));
}

#[tokio::test]
async fn stateful_websocket_rejection_is_not_retried_but_forgets_the_rejected_id() {
    let (base_url, mut frames) = spawn_websocket_server(vec![
        Reply::Frame(invalid_previous_response_id_frame()),
        Reply::Complete("resp_fresh"),
    ])
    .await;
    let session = websocket_session(base_url);
    session.state.set_latest_response_id("resp_stale");

    let events = collect(
        session
            .stream_response(transcript_request(ResponsesStateMode::Stateful))
            .await
            .expect("the websocket request should open"),
    )
    .await;

    assert!(
        matches!(
            events.last(),
            Some(Err(ProviderError::Http { status, .. })) if *status == reqwest::StatusCode::BAD_REQUEST
        ),
        "stateful mode must surface the refusal: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(ProviderEvent::MessageStarted { .. })))
    );
    assert_eq!(drain(&mut frames).len(), 1, "stateful mode must not resend");
    assert_eq!(
        session.latest_response_id(),
        None,
        "a refused id must not be offered again"
    );

    consume(
        session
            .stream_response(transcript_request(ResponsesStateMode::Stateful))
            .await
            .expect("the next request should stream"),
    )
    .await;
    let captured = drain(&mut frames);
    assert_eq!(captured.len(), 1);
    assert!(captured[0].frame.get("previous_response_id").is_none());
    assert_eq!(session.latest_response_id().as_deref(), Some("resp_fresh"));
}

#[tokio::test]
async fn hybrid_websocket_passes_other_errors_through_in_order() {
    let (base_url, mut frames) = spawn_websocket_server(vec![Reply::Frame(serde_json::json!({
        "type": "error",
        "status": 429,
        "error": {
            "type": "rate_limit_exceeded",
            "message": "slow down"
        },
        "headers": {
            "retry-after": "7"
        }
    }))])
    .await;
    let session = websocket_session(base_url);
    session.state.set_latest_response_id("resp_kept");

    let events = collect(
        session
            .stream_response(transcript_request(ResponsesStateMode::Hybrid))
            .await
            .expect("the websocket request should open"),
    )
    .await;

    // Upgrade headers, the headers the error frame echoed, then the error.
    assert_eq!(events.len(), 3, "{events:?}");
    assert!(matches!(
        &events[0],
        Ok(ProviderEvent::ResponseHeaders(headers))
            if !headers.values.iter().any(|(name, _)| name == "retry-after")
    ));
    assert!(matches!(
        &events[1],
        Ok(ProviderEvent::ResponseHeaders(headers))
            if headers.values == vec![("retry-after".to_string(), "7".to_string())]
    ));
    assert!(matches!(
        &events[2],
        Err(ProviderError::Http { status, .. }) if *status == reqwest::StatusCode::TOO_MANY_REQUESTS
    ));
    assert_eq!(
        drain(&mut frames).len(),
        1,
        "an unrelated error is not resent"
    );
    assert_eq!(session.latest_response_id().as_deref(), Some("resp_kept"));
}

/// A gateway closes a websocket left idle between requests. The next request
/// must dial a new connection rather than fail on the closed one: nothing of it
/// was delivered, so there is nothing to lose by sending it again.
#[tokio::test]
async fn a_connection_the_server_closed_while_idle_is_redialed() {
    let (base_url, mut frames) = spawn_websocket_server(vec![
        Reply::CompleteThenClose("resp_first"),
        Reply::Complete("resp_second"),
    ])
    .await;
    let session = websocket_session(base_url);

    consume(
        session
            .stream_response(transcript_request(ResponsesStateMode::ReplayOnly))
            .await
            .expect("the first request opens a connection"),
    )
    .await;
    // Give the server's close time to arrive while the connection sits idle.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let events = collect(
        session
            .stream_response(transcript_request(ResponsesStateMode::ReplayOnly))
            .await
            .expect("a closed idle connection is redialed, not used"),
    )
    .await;
    assert!(
        events.iter().all(Result::is_ok),
        "the request on the redialed connection completes: {events:?}"
    );

    let captured = drain(&mut frames);
    assert_eq!(captured.len(), 2, "each request sent once: {captured:?}");
    assert_eq!(captured[0].connection, 1);
    assert_eq!(captured[1].connection, 2, "the second request dialed anew");
}

/// The server can close the socket after a caller checked it and before the
/// frame went out. That send fails having delivered nothing, so it is the
/// call's retryable error — the runtime's request retry then dials anew —
/// rather than a failure inside a stream that has already been handed over.
#[tokio::test]
async fn a_send_on_a_closed_connection_is_a_retryable_error_of_the_call() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind websocket test server");
    let addr = listener.local_addr().expect("read websocket server addr");
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut websocket = accept_async(stream).await.expect("upgrade");
        let _ = websocket.close(None).await;
        while let Some(Ok(_)) = websocket.next().await {}
    });
    let connection = crate::responses::websocket::ResponsesWebsocketConnection::connect(
        url::Url::parse(&format!("ws://{addr}/v1/responses")).expect("url"),
        http::HeaderMap::new(),
        None,
        Duration::from_secs(5),
        None,
    )
    .await
    .expect("connect");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let error = match connection
        .stream_request(serde_json::json!({"model": "gpt-5"}), true)
        .await
    {
        Ok(_) => panic!("a send on a closed socket must fail the call"),
        Err(error) => error,
    };
    assert!(
        matches!(error, ProviderError::Retryable { .. }),
        "{error:?}"
    );
    assert!(connection.is_closed().await, "the dead socket is dropped");
}

#[tokio::test]
async fn dropping_an_idle_websocket_response_allows_the_next_turn_to_redial() {
    let (base_url, mut frames) =
        spawn_websocket_server(vec![Reply::Silent, Reply::Complete("resp_next")]).await;
    let session = websocket_session(base_url);
    let mut cancelled = session
        .stream_response(transcript_request(ResponsesStateMode::ReplayOnly))
        .await
        .expect("the abandoned request is sent");
    assert!(matches!(
        cancelled.recv().await,
        Some(Ok(ProviderEvent::ResponseHeaders(_)))
    ));
    drop(cancelled);

    let events = tokio::time::timeout(Duration::from_secs(1), async {
        collect(
            session
                .stream_response(transcript_request(ResponsesStateMode::ReplayOnly))
                .await
                .expect("the next turn opens a fresh connection"),
        )
        .await
    })
    .await
    .expect("closing the receiver releases the socket before its idle timeout");
    assert!(events.iter().all(Result::is_ok), "{events:?}");
    assert!(events.iter().any(|event| matches!(
        event,
        Ok(ProviderEvent::MessageStarted { id, .. }) if id == "resp_next"
    )));
    let captured = drain(&mut frames);
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0].connection, 1);
    assert_eq!(captured[1].connection, 2);
}

#[tokio::test]
async fn a_dropped_started_turn_cannot_replace_the_last_completed_websocket_chain_head() {
    for mode in [ResponsesStateMode::Stateful, ResponsesStateMode::Hybrid] {
        let (base_url, mut frames) = spawn_websocket_server(vec![
            Reply::Complete("resp_kept"),
            Reply::Frame(serde_json::json!({
                "type": "response.created",
                "response": {"id": "resp_cancelled", "model": "gpt-5", "status": "in_progress"}
            })),
            Reply::Complete("resp_next"),
        ])
        .await;
        let session = websocket_session(base_url);
        consume(
            session
                .stream_response(transcript_request(mode))
                .await
                .expect("the first turn opens"),
        )
        .await;
        assert_eq!(session.latest_response_id().as_deref(), Some("resp_kept"));

        let mut cancelled = session
            .stream_response(transcript_request(mode))
            .await
            .expect("the abandoned turn opens");
        loop {
            let event = cancelled.recv().await.expect("the turn has a start");
            if matches!(
                event,
                Ok(ProviderEvent::MessageStarted { id, .. }) if id == "resp_cancelled"
            ) {
                break;
            }
        }
        drop(cancelled);

        tokio::time::timeout(Duration::from_secs(1), async {
            consume(
                session
                    .stream_response(transcript_request(mode))
                    .await
                    .expect("the next turn opens after cancellation"),
            )
            .await;
        })
        .await
        .expect("closing the receiver releases the socket before its idle timeout");

        let captured = drain(&mut frames);
        assert_eq!(captured.len(), 3);
        assert_eq!(captured[0].connection, 1);
        assert_eq!(
            captured[1].connection, 1,
            "a completed socket remains reusable"
        );
        assert_eq!(captured[1].frame["previous_response_id"], "resp_kept");
        assert_eq!(captured[2].connection, 2, "the abandoned socket is retired");
        assert_eq!(captured[2].frame["previous_response_id"], "resp_kept");
        assert_eq!(captured[2].frame["input"], captured[1].frame["input"]);
        assert_eq!(session.latest_response_id().as_deref(), Some("resp_next"));
    }
}

fn websocket_session(base_url: String) -> ResponsesSession<StaticCredentialSource> {
    let mut definition = super::super::openai_definition();
    definition.base_url = Some(base_url);
    // Bounds a test that regresses into waiting for a reply that never comes.
    definition.stream_idle_timeout = Duration::from_secs(5);
    ResponsesProvider::with_shared_credential_source(
        definition,
        Arc::new(StaticCredentialSource::new("test-key")),
    )
    .session()
}

fn transcript_request(state_mode: ResponsesStateMode) -> Request<'static> {
    Request {
        model: Cow::Borrowed("gpt-5"),
        system: None,
        messages: Cow::Owned(vec![
            crate::Message::user(crate::ContentBlock::text("first question")),
            crate::Message::assistant(crate::ContentBlock::text("first answer")),
            crate::Message::user(crate::ContentBlock::text("follow-up")),
        ]),
        tools: Cow::Owned(Vec::new()),
        tool_choice: None,
        temperature: None,
        max_output_tokens: None,
        metadata: Cow::Owned(BTreeMap::new()),
        provider_request_options: ProviderRequestOptions {
            responses: crate::ResponsesRequestOptions {
                transport: ResponsesTransport::WebSocket,
                state_mode,
                ..Default::default()
            },
            ..ProviderRequestOptions::default()
        },
    }
}

async fn collect(mut stream: ProviderEventStream) -> Vec<Result<ProviderEvent, ProviderError>> {
    let mut events = Vec::new();
    while let Some(event) = stream.recv().await {
        events.push(event);
    }
    events
}

async fn consume(stream: ProviderEventStream) {
    for event in collect(stream).await {
        event.expect("stream event should decode");
    }
}

/// What the server answers to the next `response.create` frame it reads.
enum Reply {
    /// Keep the connection alive without emitting an event.
    Silent,
    /// Send this one frame and nothing else.
    Frame(serde_json::Value),
    /// Create and complete a response with this id.
    Complete(&'static str),
    /// Complete a response with this id, then close the connection the way
    /// a gateway closes one left idle between requests.
    CompleteThenClose(&'static str),
}

impl Reply {
    fn frames(self) -> Vec<serde_json::Value> {
        match self {
            Self::Silent => Vec::new(),
            Self::Frame(frame) => vec![frame],
            Self::Complete(id) | Self::CompleteThenClose(id) => {
                ["response.created", "response.completed"]
                    .into_iter()
                    .zip(["in_progress", "completed"])
                    .map(|(kind, status)| {
                        serde_json::json!({
                            "type": kind,
                            "response": {"id": id, "model": "gpt-5", "status": status}
                        })
                    })
                    .collect()
            }
        }
    }
}

#[derive(Debug)]
struct CapturedFrame {
    connection: usize,
    frame: serde_json::Value,
}

/// A websocket server that answers request frames, across however many
/// connections the client opens, with `replies` in order. A frame beyond the
/// script closes its connection instead of hanging the client.
async fn spawn_websocket_server(
    replies: Vec<Reply>,
) -> (String, mpsc::UnboundedReceiver<CapturedFrame>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind websocket test server");
    let addr = listener.local_addr().expect("read websocket server addr");
    let replies = Arc::new(tokio::sync::Mutex::new(VecDeque::from(replies)));
    let (tx_frame, rx_frame) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        let mut connection = 0;
        while let Ok((stream, _)) = listener.accept().await {
            connection += 1;
            tokio::spawn(serve_connection(
                stream,
                connection,
                Arc::clone(&replies),
                tx_frame.clone(),
            ));
        }
    });

    (format!("http://{addr}/v1"), rx_frame)
}

async fn serve_connection(
    stream: tokio::net::TcpStream,
    connection: usize,
    replies: Arc<tokio::sync::Mutex<VecDeque<Reply>>>,
    tx_frame: mpsc::UnboundedSender<CapturedFrame>,
) {
    let Ok(mut websocket) = accept_async(stream).await else {
        return;
    };
    while let Some(Ok(message)) = websocket.next().await {
        let WsMessage::Text(text) = message else {
            continue;
        };
        let frame = serde_json::from_str(&text).expect("request frame should be json");
        let _ = tx_frame.send(CapturedFrame { connection, frame });
        let Some(reply) = replies.lock().await.pop_front() else {
            let _ = websocket.close(None).await;
            return;
        };
        let close_after = matches!(reply, Reply::CompleteThenClose(_));
        for frame in reply.frames() {
            if websocket
                .send(WsMessage::Text(frame.to_string().into()))
                .await
                .is_err()
            {
                return;
            }
        }
        if close_after {
            let _ = websocket.close(None).await;
            // Drain until the client's close reply, so the close handshake
            // completes rather than the socket being reset.
            while let Some(Ok(_)) = websocket.next().await {}
            return;
        }
    }
}

fn drain(frames: &mut mpsc::UnboundedReceiver<CapturedFrame>) -> Vec<CapturedFrame> {
    std::iter::from_fn(|| frames.try_recv().ok()).collect()
}
