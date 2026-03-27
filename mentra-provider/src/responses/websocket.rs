use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use futures_util::SinkExt;
use futures_util::StreamExt;
use http::HeaderMap;
use http::HeaderValue;
use serde::Deserialize;
use serde_json::Value;
use serde_json::map::Map as JsonMap;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use url::Url;

use crate::ProviderError;
use crate::ProviderEvent;
use crate::ProviderEventStream;
use crate::ResponseHeaders;

use super::sse::StreamState;
use super::sse::parse_json_event;

const X_CODEX_TURN_STATE_HEADER: &str = "x-codex-turn-state";
const WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE: &str = "websocket_connection_limit_reached";
const WEBSOCKET_CONNECTION_LIMIT_REACHED_MESSAGE: &str = "Responses websocket connection limit reached (60 minutes). Create a new websocket connection to continue.";

pub trait ResponsesWebsocketTelemetry: Send + Sync {
    fn on_ws_request(
        &self,
        duration: Duration,
        error: Option<&ProviderError>,
        connection_reused: bool,
    );

    fn on_ws_event(
        &self,
        result: &Result<Option<Result<Message, WsError>>, ProviderError>,
        duration: Duration,
    );
}

struct WsStream {
    tx_command: mpsc::Sender<WsCommand>,
    rx_message: mpsc::UnboundedReceiver<Result<Message, WsError>>,
    pump_task: tokio::task::JoinHandle<()>,
}

enum WsCommand {
    Send {
        message: Message,
        tx_result: oneshot::Sender<Result<(), WsError>>,
    },
}

impl WsStream {
    fn new(inner: WebSocketStream<MaybeTlsStream<TcpStream>>) -> Self {
        let (tx_command, mut rx_command) = mpsc::channel::<WsCommand>(32);
        let (tx_message, rx_message) = mpsc::unbounded_channel::<Result<Message, WsError>>();

        let pump_task = tokio::spawn(async move {
            let mut inner = inner;
            loop {
                tokio::select! {
                    command = rx_command.recv() => {
                        let Some(command) = command else {
                            break;
                        };
                        match command {
                            WsCommand::Send { message, tx_result } => {
                                let result = inner.send(message).await;
                                let should_break = result.is_err();
                                let _ = tx_result.send(result);
                                if should_break {
                                    break;
                                }
                            }
                        }
                    }
                    message = inner.next() => {
                        let Some(message) = message else {
                            break;
                        };
                        match message {
                            Ok(Message::Ping(payload)) => {
                                if let Err(err) = inner.send(Message::Pong(payload)).await {
                                    let _ = tx_message.send(Err(err));
                                    break;
                                }
                            }
                            Ok(Message::Pong(_)) => {}
                            Ok(message @ (Message::Text(_)
                            | Message::Binary(_)
                            | Message::Close(_)
                            | Message::Frame(_))) => {
                                let is_close = matches!(message, Message::Close(_));
                                if tx_message.send(Ok(message)).is_err() {
                                    break;
                                }
                                if is_close {
                                    break;
                                }
                            }
                            Err(err) => {
                                let _ = tx_message.send(Err(err));
                                break;
                            }
                        }
                    }
                }
            }
        });

        Self {
            tx_command,
            rx_message,
            pump_task,
        }
    }

    async fn send(&self, message: Message) -> Result<(), WsError> {
        let (tx_result, rx_result) = oneshot::channel();
        if self
            .tx_command
            .send(WsCommand::Send { message, tx_result })
            .await
            .is_err()
        {
            return Err(WsError::ConnectionClosed);
        }
        rx_result.await.unwrap_or(Err(WsError::ConnectionClosed))
    }

    async fn next(&mut self) -> Option<Result<Message, WsError>> {
        self.rx_message.recv().await
    }
}

impl Drop for WsStream {
    fn drop(&mut self) {
        self.pump_task.abort();
    }
}

pub struct ResponsesWebsocketConnection {
    stream: Arc<Mutex<Option<WsStream>>>,
    idle_timeout: Duration,
    response_headers: ResponseHeaders,
    telemetry: Option<Arc<dyn ResponsesWebsocketTelemetry>>,
}

impl std::fmt::Debug for ResponsesWebsocketConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponsesWebsocketConnection")
            .field("stream", &"<ws-stream>")
            .field("idle_timeout", &self.idle_timeout)
            .field("response_headers", &self.response_headers)
            .field("telemetry", &self.telemetry.as_ref().map(|_| "<telemetry>"))
            .finish()
    }
}

impl ResponsesWebsocketConnection {
    pub async fn connect(
        url: Url,
        headers: HeaderMap,
        turn_state: Option<Arc<OnceLock<String>>>,
        idle_timeout: Duration,
        telemetry: Option<Arc<dyn ResponsesWebsocketTelemetry>>,
    ) -> Result<Self, ProviderError> {
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|error| ProviderError::InvalidRequest(error.to_string()))?;
        request.headers_mut().extend(headers);

        let (stream, response) = connect_async(request)
            .await
            .map_err(|error| map_ws_error(error, &url))?;

        if let Some(turn_state) = turn_state
            && let Some(header_value) = response
                .headers()
                .get(X_CODEX_TURN_STATE_HEADER)
                .and_then(|value| value.to_str().ok())
        {
            let _ = turn_state.set(header_value.to_string());
        }

        let response_headers = ResponseHeaders {
            values: response
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.as_str().to_string(), value.to_string()))
                })
                .collect(),
        };

        Ok(Self {
            stream: Arc::new(Mutex::new(Some(WsStream::new(stream)))),
            idle_timeout,
            response_headers,
            telemetry,
        })
    }

    pub async fn is_closed(&self) -> bool {
        self.stream.lock().await.is_none()
    }

    pub async fn stream_request(
        &self,
        request_body: Value,
        connection_reused: bool,
    ) -> Result<ProviderEventStream, ProviderError> {
        let (tx_event, rx_event) =
            mpsc::unbounded_channel::<Result<ProviderEvent, ProviderError>>();
        let stream = Arc::clone(&self.stream);
        let idle_timeout = self.idle_timeout;
        let response_headers = self.response_headers.clone();
        let telemetry = self.telemetry.clone();

        let request_text =
            serde_json::to_string(&request_body).map_err(ProviderError::Serialize)?;

        tokio::spawn(async move {
            if tx_event
                .send(Ok(ProviderEvent::ResponseHeaders(response_headers)))
                .is_err()
            {
                return;
            }

            let mut guard = stream.lock().await;
            let result = {
                let Some(ws_stream) = guard.as_mut() else {
                    let _ = tx_event.send(Err(ProviderError::MalformedStream(
                        "websocket connection is closed".to_string(),
                    )));
                    return;
                };
                run_websocket_response_stream(
                    ws_stream,
                    tx_event.clone(),
                    request_text,
                    idle_timeout,
                    telemetry,
                    connection_reused,
                )
                .await
            };

            if let Err(err) = result {
                let failed_stream = guard.take();
                drop(guard);
                drop(failed_stream);
                let _ = tx_event.send(Err(err));
            }
        });

        Ok(rx_event)
    }
}

pub fn merge_request_headers(
    provider_headers: &HeaderMap,
    extra_headers: HeaderMap,
    default_headers: HeaderMap,
) -> HeaderMap {
    let mut headers = provider_headers.clone();
    headers.extend(extra_headers);
    for (name, value) in &default_headers {
        if let http::header::Entry::Vacant(entry) = headers.entry(name) {
            entry.insert(value.clone());
        }
    }
    headers
}

async fn run_websocket_response_stream(
    ws_stream: &mut WsStream,
    tx_event: mpsc::UnboundedSender<Result<ProviderEvent, ProviderError>>,
    request_text: String,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn ResponsesWebsocketTelemetry>>,
    connection_reused: bool,
) -> Result<(), ProviderError> {
    let request_start = Instant::now();
    let send_result = ws_stream.send(Message::Text(request_text.into())).await;
    let send_error = send_result
        .as_ref()
        .err()
        .map(|error| ProviderError::MalformedStream(error.to_string()));
    if let Some(t) = telemetry.as_ref() {
        t.on_ws_request(
            request_start.elapsed(),
            send_error.as_ref(),
            connection_reused,
        );
    }
    send_result.map_err(|error| ProviderError::MalformedStream(error.to_string()))?;

    let mut state = StreamState::default();
    loop {
        let poll_start = Instant::now();
        let message_result = tokio::time::timeout(idle_timeout, ws_stream.next())
            .await
            .map_err(|_| {
                ProviderError::MalformedStream("idle timeout waiting for websocket".into())
            });
        if let Some(t) = telemetry.as_ref() {
            t.on_ws_event(&message_result, poll_start.elapsed());
        }
        let message = match message_result {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(error))) => {
                return Err(ProviderError::MalformedStream(error.to_string()));
            }
            Ok(None) => {
                return Err(ProviderError::MalformedStream(
                    "stream closed before response.completed".to_string(),
                ));
            }
            Err(error) => return Err(error),
        };

        match message {
            Message::Text(text) => {
                if let Some(mapped) = parse_wrapped_websocket_error_event(&text) {
                    if let Some(headers) = mapped.headers
                        && tx_event
                            .send(Ok(ProviderEvent::ResponseHeaders(headers)))
                            .is_err()
                    {
                        return Ok(());
                    }
                    return Err(mapped.error);
                }

                let mut saw_message_stopped = false;
                for event in parse_json_event(&text, &mut state)? {
                    if matches!(event, ProviderEvent::MessageStopped) {
                        saw_message_stopped = true;
                    }
                    if tx_event.send(Ok(event)).is_err() {
                        return Ok(());
                    }
                }
                if saw_message_stopped {
                    break;
                }
            }
            Message::Binary(_) => {
                return Err(ProviderError::MalformedStream(
                    "unexpected binary websocket event".to_string(),
                ));
            }
            Message::Close(_) => {
                return Err(ProviderError::MalformedStream(
                    "websocket closed by server before response.completed".to_string(),
                ));
            }
            Message::Frame(_) => {}
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }

    Ok(())
}

fn map_ws_error(error: WsError, url: &Url) -> ProviderError {
    match error {
        WsError::Http(response) => {
            let status = response.status();
            let body = response
                .body()
                .as_ref()
                .and_then(|bytes| String::from_utf8(bytes.clone()).ok())
                .unwrap_or_else(|| format!("websocket connection failed for {url}"));
            ProviderError::Http { status, body }
        }
        WsError::ConnectionClosed | WsError::AlreadyClosed => {
            ProviderError::MalformedStream("websocket closed".to_string())
        }
        WsError::Io(error) => ProviderError::InvalidResponse(error.to_string()),
        other => ProviderError::InvalidResponse(other.to_string()),
    }
}

#[derive(Debug)]
struct MappedWebsocketError {
    error: ProviderError,
    headers: Option<ResponseHeaders>,
}

#[derive(Debug, Deserialize)]
struct WrappedWebsocketError {
    code: Option<String>,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WrappedWebsocketErrorEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(alias = "status_code")]
    status: Option<u16>,
    #[serde(default)]
    error: Option<WrappedWebsocketError>,
    #[serde(default)]
    headers: Option<JsonMap<String, Value>>,
}

fn parse_wrapped_websocket_error_event(payload: &str) -> Option<MappedWebsocketError> {
    let event: WrappedWebsocketErrorEvent = serde_json::from_str(payload).ok()?;
    if event.kind != "error" {
        return None;
    }

    if let Some(error) = event.error.as_ref()
        && let Some(code) = error.code.as_deref()
        && code == WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE
    {
        return Some(MappedWebsocketError {
            error: ProviderError::Retryable {
                message: error
                    .message
                    .clone()
                    .unwrap_or_else(|| WEBSOCKET_CONNECTION_LIMIT_REACHED_MESSAGE.to_string()),
                delay: None,
            },
            headers: event.headers.map(response_headers_from_json),
        });
    }

    let status = reqwest::StatusCode::from_u16(event.status?).ok()?;
    let body = payload.to_string();
    Some(MappedWebsocketError {
        error: ProviderError::Http { status, body },
        headers: event.headers.map(response_headers_from_json),
    })
}

fn response_headers_from_json(headers: JsonMap<String, Value>) -> ResponseHeaders {
    ResponseHeaders {
        values: headers
            .into_iter()
            .filter_map(|(name, value)| {
                json_header_value(value)
                    .and_then(|value| value.to_str().ok().map(|value| (name, value.to_string())))
            })
            .collect(),
    }
}

fn json_header_value(value: Value) -> Option<HeaderValue> {
    let value = match value {
        Value::String(value) => value,
        Value::Number(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
        _ => return None,
    };
    HeaderValue::from_str(&value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merge_request_headers_matches_http_precedence() {
        let mut provider_headers = HeaderMap::new();
        provider_headers.insert(
            "originator",
            HeaderValue::from_static("provider-originator"),
        );
        provider_headers.insert("x-priority", HeaderValue::from_static("provider"));

        let mut extra_headers = HeaderMap::new();
        extra_headers.insert("x-priority", HeaderValue::from_static("extra"));

        let mut default_headers = HeaderMap::new();
        default_headers.insert("originator", HeaderValue::from_static("default-originator"));
        default_headers.insert("x-priority", HeaderValue::from_static("default"));
        default_headers.insert("x-default-only", HeaderValue::from_static("default-only"));

        let merged = merge_request_headers(&provider_headers, extra_headers, default_headers);

        assert_eq!(
            merged.get("originator"),
            Some(&HeaderValue::from_static("provider-originator"))
        );
        assert_eq!(
            merged.get("x-priority"),
            Some(&HeaderValue::from_static("extra"))
        );
        assert_eq!(
            merged.get("x-default-only"),
            Some(&HeaderValue::from_static("default-only"))
        );
    }

    #[test]
    fn wrapped_websocket_error_preserves_rate_limit_headers() {
        let payload = json!({
            "type": "error",
            "status": 429,
            "error": {
                "type": "usage_limit_reached",
                "message": "The usage limit has been reached"
            },
            "headers": {
                "x-codex-primary-used-percent": "100.0"
            }
        })
        .to_string();

        let mapped = parse_wrapped_websocket_error_event(&payload)
            .expect("error payload should map to a provider error");
        let ProviderError::Http { status, .. } = mapped.error else {
            panic!("expected ProviderError::Http");
        };
        assert_eq!(status, reqwest::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            mapped
                .headers
                .expect("rate limit headers should be present")
                .values,
            vec![(
                "x-codex-primary-used-percent".to_string(),
                "100.0".to_string()
            )]
        );
    }

    #[test]
    fn websocket_connection_limit_maps_to_retryable() {
        let payload = json!({
            "type": "error",
            "status": 400,
            "error": {
                "type": "invalid_request_error",
                "code": "websocket_connection_limit_reached",
                "message": WEBSOCKET_CONNECTION_LIMIT_REACHED_MESSAGE
            }
        })
        .to_string();

        let mapped = parse_wrapped_websocket_error_event(&payload)
            .expect("connection limit payload should map");
        let ProviderError::Retryable { message, delay } = mapped.error else {
            panic!("expected ProviderError::Retryable");
        };
        assert_eq!(message, WEBSOCKET_CONNECTION_LIMIT_REACHED_MESSAGE);
        assert_eq!(delay, None);
    }
}
