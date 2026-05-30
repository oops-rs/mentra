use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use http::HeaderMap;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::oneshot::error::TryRecvError;
use url::Url;

use crate::CompactionRequest;
use crate::CompactionResponse;
use crate::CredentialSource;
use crate::MemorySummarizeRequest;
use crate::MemorySummarizeResponse;
use crate::ModelInfo;
use crate::ProviderCredentials;
use crate::ProviderDefinition;
use crate::ProviderError;
use crate::ProviderEvent;
use crate::ProviderEventStream;
use crate::ProviderSession;
use crate::Request;
use crate::Response;
use crate::ResponsesTransport;
use crate::SessionRequestOptions;
use crate::request::ResponsesRequestCompression;

use super::SharedTurnState;
use super::model::ResponsesModelsPage;
use super::model::ResponsesRequest;
use super::sse::spawn_event_stream;
use super::websocket::ResponsesWebsocketConnection;
use super::websocket::ResponsesWebsocketTelemetry;
use super::websocket::merge_request_headers;
use super::websocket::response_create_frame;

/// Session-scoped Responses transport state.
///
/// This is intentionally lightweight and keeps the pieces needed for websocket prewarm and
/// HTTP fallback without binding the provider to any higher-level runtime.
pub struct ResponsesSession<C> {
    definition: ProviderDefinition,
    credential_source: Arc<C>,
    client: reqwest::Client,
    state: Arc<ResponsesSessionState>,
}

#[derive(Default)]
struct WebsocketSession {
    connection_reused: StdMutex<bool>,
    connection: Option<ResponsesWebsocketConnection>,
    _last_request: Option<ResponsesRequest>,
    last_response_rx: Option<oneshot::Receiver<Response>>,
}

impl WebsocketSession {
    fn set_connection_reused(&self, connection_reused: bool) {
        *self
            .connection_reused
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = connection_reused;
    }

    fn connection_reused(&self) -> bool {
        *self
            .connection_reused
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn connection(&self) -> Option<ResponsesWebsocketConnection> {
        self.connection.clone()
    }

    fn store_connection(&mut self, connection: ResponsesWebsocketConnection) {
        self.connection = Some(connection);
    }

    fn clear_connection(&mut self) {
        self.connection = None;
    }
}

pub(crate) struct ResponsesSessionState {
    disable_websockets: AtomicBool,
    websocket_session: StdMutex<WebsocketSession>,
    turn_state: SharedTurnState,
    latest_response_id: StdMutex<Option<String>>,
}

impl Default for ResponsesSessionState {
    fn default() -> Self {
        Self {
            disable_websockets: AtomicBool::new(false),
            websocket_session: StdMutex::new(WebsocketSession::default()),
            turn_state: Arc::new(StdMutex::new(None)),
            latest_response_id: StdMutex::new(None),
        }
    }
}

impl ResponsesSessionState {
    fn turn_state(&self) -> Option<String> {
        self.turn_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_turn_state(&self, turn_state: impl Into<String>) {
        *self
            .turn_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(turn_state.into());
    }

    fn latest_response_id(&self) -> Option<String> {
        self.latest_response_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_latest_response_id(&self, response_id: impl Into<String>) {
        *self
            .latest_response_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(response_id.into());
    }

    fn clear_latest_response_id(&self) {
        *self
            .latest_response_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

impl<C> ResponsesSession<C>
where
    C: CredentialSource + 'static,
{
    pub(crate) fn new(
        definition: ProviderDefinition,
        credential_source: Arc<C>,
        client: reqwest::Client,
        state: Arc<ResponsesSessionState>,
    ) -> Self {
        Self {
            definition,
            credential_source,
            client,
            state,
        }
    }

    pub fn websocket_connect_timeout(&self) -> Duration {
        self.definition.websocket_connect_timeout
    }

    pub fn stream_idle_timeout(&self) -> Duration {
        self.definition.stream_idle_timeout
    }

    pub fn websocket_url_for_path(&self, path: &str) -> Result<Url, ProviderError> {
        self.definition
            .websocket_url_for_path(path)
            .map_err(|error| ProviderError::InvalidRequest(error.to_string()))
    }

    pub fn request_url_for_path(&self, path: &str) -> Result<Url, ProviderError> {
        Url::parse(&self.definition.url_for_path(path))
            .map_err(|error| ProviderError::InvalidRequest(error.to_string()))
    }

    fn responses_endpoint_path(&self) -> &'static str {
        responses_endpoint_path_for_base(self.definition.base_url.as_deref())
    }

    pub fn disable_websockets(&self) {
        self.state.disable_websockets.store(true, Ordering::Relaxed);
    }

    pub fn websockets_enabled(&self) -> bool {
        self.definition.capabilities.supports_websockets
            && !self.state.disable_websockets.load(Ordering::Relaxed)
    }

    pub fn set_connection_reused(&self, connection_reused: bool) {
        self.state
            .websocket_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_connection_reused(connection_reused);
    }

    pub fn connection_reused(&self) -> bool {
        self.state
            .websocket_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .connection_reused()
    }

    pub fn build_websocket_headers(
        &self,
        credentials: &ProviderCredentials,
        turn_metadata_header: Option<&str>,
    ) -> Result<HeaderMap, ProviderError> {
        let session = SessionRequestOptions {
            sticky_turn_state: self.state.turn_state(),
            turn_metadata: turn_metadata_header.map(str::to_string),
            subagent: None,
            prefer_connection_reuse: Some(self.connection_reused()),
            session_affinity: None,
            extra_headers: std::collections::BTreeMap::new(),
        };
        self.build_websocket_headers_for_session(credentials, Some(&session))
    }

    pub fn build_websocket_headers_for_session(
        &self,
        credentials: &ProviderCredentials,
        session: Option<&SessionRequestOptions>,
    ) -> Result<HeaderMap, ProviderError> {
        let fallback_turn_state = self.state.turn_state();
        self.definition.build_headers_for_session(
            credentials,
            session,
            fallback_turn_state.as_deref(),
        )
    }

    pub fn set_turn_state(&self, turn_state: impl Into<String>) -> bool {
        self.state.set_turn_state(turn_state);
        true
    }

    pub fn turn_state(&self) -> Option<String> {
        self.state.turn_state()
    }

    pub fn latest_response_id(&self) -> Option<String> {
        self.state.latest_response_id()
    }

    pub fn clear_latest_response_id(&self) {
        self.state.clear_latest_response_id();
    }

    pub async fn websocket_connection_is_closed(&self) -> bool {
        let connection = self
            .state
            .websocket_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .connection();
        match connection {
            Some(connection) => connection.is_closed().await,
            None => true,
        }
    }

    pub async fn connect_websocket(
        &self,
        extra_headers: HeaderMap,
        default_headers: HeaderMap,
        turn_state: Option<SharedTurnState>,
        telemetry: Option<Arc<dyn ResponsesWebsocketTelemetry>>,
    ) -> Result<(), ProviderError> {
        let credentials = self.credential_source.credentials().await?;
        let provider_headers = self.definition.build_headers(&credentials)?;
        let headers = merge_request_headers(&provider_headers, extra_headers, default_headers);
        let url = self
            .definition
            .websocket_url_with_auth_for_path(self.responses_endpoint_path(), &credentials)?;
        let connection = ResponsesWebsocketConnection::connect(
            url,
            headers,
            turn_state.or_else(|| Some(Arc::clone(&self.state.turn_state))),
            self.stream_idle_timeout(),
            telemetry,
        )
        .await?;
        self.state
            .websocket_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .store_connection(connection);
        Ok(())
    }

    pub async fn stream_websocket_request(
        &self,
        request_body: serde_json::Value,
    ) -> Result<ProviderEventStream, ProviderError> {
        let (connection, connection_reused) = {
            let websocket_session = self
                .state
                .websocket_session
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                websocket_session.connection(),
                websocket_session.connection_reused(),
            )
        };
        let Some(connection) = connection else {
            return Err(ProviderError::MalformedStream(
                "websocket connection is unavailable".to_string(),
            ));
        };
        connection
            .stream_request(request_body, connection_reused)
            .await
    }

    pub fn clear_websocket_connection(&self) {
        self.state
            .websocket_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear_connection();
    }

    pub async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let credentials = self.credential_source.credentials().await?;
        let response = self
            .client
            .get(
                self.definition
                    .request_url_with_auth_for_path("v1/models", &credentials)?,
            )
            .headers(self.definition.build_headers(&credentials)?)
            .send()
            .await
            .map_err(ProviderError::Transport)?;

        if !response.status().is_success() {
            return Err(ProviderError::Http {
                status: response.status(),
                body: response.text().await.unwrap_or_default(),
            });
        }

        let models = response
            .json::<ResponsesModelsPage>()
            .await
            .map_err(ProviderError::Decode)?;

        Ok(models.into_model_info(self.definition.descriptor.id.clone()))
    }

    pub async fn stream_response<'a>(
        &self,
        mut request: Request<'a>,
    ) -> Result<ProviderEventStream, ProviderError> {
        let provider_name = self
            .definition
            .descriptor
            .display_name
            .as_deref()
            .unwrap_or(self.definition.descriptor.id.as_str());
        let session = request.provider_request_options.session.clone();
        let state_mode = request.provider_request_options.responses.state_mode;
        let transport = request.provider_request_options.responses.transport;
        if request
            .provider_request_options
            .responses
            .previous_response_id
            .is_none()
            && state_mode.uses_provider_state()
        {
            request
                .provider_request_options
                .responses
                .previous_response_id = self.state.latest_response_id();
        }
        let compression = request.provider_request_options.responses.compression;
        let request = ResponsesRequest::try_from_request(request, provider_name)?;
        let credentials = self.credential_source.credentials().await?;

        match transport {
            ResponsesTransport::HttpSse => {
                self.stream_http_response(request, compression, &credentials, &session, state_mode)
                    .await
            }
            ResponsesTransport::WebSocket => {
                self.stream_websocket_response(request, &credentials, &session)
                    .await
            }
        }
    }

    async fn stream_http_response(
        &self,
        mut request: ResponsesRequest,
        compression: ResponsesRequestCompression,
        credentials: &ProviderCredentials,
        session: &SessionRequestOptions,
        state_mode: crate::ResponsesStateMode,
    ) -> Result<ProviderEventStream, ProviderError> {
        let response = self
            .send_http_responses_request(&request, compression, credentials, session)
            .await?;

        if !response.status().is_success() {
            let error = ProviderError::Http {
                status: response.status(),
                body: response.text().await.unwrap_or_default(),
            };
            if state_mode == crate::ResponsesStateMode::Hybrid
                && request.previous_response_id().is_some()
                && previous_response_state_rejected(&error)
            {
                self.state.clear_latest_response_id();
                request.clear_previous_response_id();
                let response = self
                    .send_http_responses_request(&request, compression, credentials, session)
                    .await?;
                if !response.status().is_success() {
                    return Err(ProviderError::Http {
                        status: response.status(),
                        body: response.text().await.unwrap_or_default(),
                    });
                }
                return Ok(self.track_response_state(spawn_event_stream(response)));
            }
            return Err(error);
        }

        if let Some(turn_state) = response
            .headers()
            .get("x-codex-turn-state")
            .and_then(|value| value.to_str().ok())
        {
            self.state.set_turn_state(turn_state);
        }

        Ok(self.track_response_state(spawn_event_stream(response)))
    }

    async fn stream_websocket_response(
        &self,
        request: ResponsesRequest,
        credentials: &ProviderCredentials,
        session: &SessionRequestOptions,
    ) -> Result<ProviderEventStream, ProviderError> {
        if !self.websockets_enabled() {
            return Err(ProviderError::UnsupportedCapability(
                "responses_websocket".to_string(),
            ));
        }

        if self.websocket_connection_is_closed().await {
            let headers = self.build_websocket_headers_for_session(credentials, Some(session))?;
            let url = self
                .definition
                .websocket_url_with_auth_for_path(self.responses_endpoint_path(), credentials)?;
            let connection = ResponsesWebsocketConnection::connect(
                url,
                headers,
                Some(Arc::clone(&self.state.turn_state)),
                self.stream_idle_timeout(),
                None,
            )
            .await?;
            self.state
                .websocket_session
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .store_connection(connection);
        }

        let response = serde_json::to_value(request).map_err(ProviderError::Serialize)?;
        let stream = self
            .stream_websocket_request(response_create_frame(response))
            .await?;
        Ok(self.track_response_state(stream))
    }

    pub async fn send_response<'a>(&self, request: Request<'a>) -> Result<Response, ProviderError> {
        crate::collect_response_from_stream(self.stream_response(request).await?).await
    }

    async fn send_http_responses_request(
        &self,
        request: &ResponsesRequest,
        compression: ResponsesRequestCompression,
        credentials: &ProviderCredentials,
        session: &SessionRequestOptions,
    ) -> Result<reqwest::Response, ProviderError> {
        let request_builder = self
            .client
            .post(
                self.definition
                    .request_url_with_auth_for_path(self.responses_endpoint_path(), credentials)?,
            )
            .headers(self.build_http_headers_for_session(credentials, Some(session))?)
            .header(reqwest::header::ACCEPT, "text/event-stream");

        match compression {
            ResponsesRequestCompression::None => request_builder
                .json(request)
                .send()
                .await
                .map_err(ProviderError::Transport),
            ResponsesRequestCompression::Zstd => {
                let body = serde_json::to_vec(request).map_err(ProviderError::Serialize)?;
                let compressed =
                    zstd::stream::encode_all(std::io::Cursor::new(body), 3).map_err(|error| {
                        ProviderError::InvalidRequest(format!(
                            "failed to compress responses request: {error}"
                        ))
                    })?;
                request_builder
                    .header(reqwest::header::CONTENT_ENCODING, "zstd")
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(compressed)
                    .send()
                    .await
                    .map_err(ProviderError::Transport)
            }
        }
    }

    fn track_response_state(&self, mut stream: ProviderEventStream) -> ProviderEventStream {
        let (tx, rx) = mpsc::unbounded_channel();
        let state = Arc::clone(&self.state);

        tokio::spawn(async move {
            while let Some(event) = stream.recv().await {
                if let Ok(ProviderEvent::MessageStarted { id, .. }) = &event {
                    state.set_latest_response_id(id.clone());
                }

                if tx.send(event).is_err() {
                    break;
                }
            }
        });

        rx
    }

    pub fn take_turn_state(&self) -> SharedTurnState {
        Arc::clone(&self.state.turn_state)
    }

    pub fn last_response_rx_ready(&self) -> bool {
        let mut session = self
            .state
            .websocket_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        session
            .last_response_rx
            .as_mut()
            .is_some_and(|rx| matches!(rx.try_recv(), Ok(_) | Err(TryRecvError::Closed)))
    }

    fn build_http_headers_for_session(
        &self,
        credentials: &ProviderCredentials,
        session: Option<&SessionRequestOptions>,
    ) -> Result<HeaderMap, ProviderError> {
        let mut headers = self.build_websocket_headers_for_session(credentials, session)?;
        if let Some(session_id) = session.and_then(|session| session.session_affinity.as_deref())
            && let Ok(value) = http::HeaderValue::from_str(session_id)
        {
            headers.insert("x-client-request-id", value.clone());
            headers.insert("session_id", value);
        }
        if let Some(subagent) = session.and_then(|session| session.subagent.as_deref())
            && let Ok(value) = http::HeaderValue::from_str(subagent)
        {
            headers.insert("x-openai-subagent", value);
        }
        Ok(headers)
    }
}

fn responses_endpoint_path_for_base(base_url: Option<&str>) -> &'static str {
    let Some(base_url) = base_url else {
        return "v1/responses";
    };
    let Ok(url) = Url::parse(base_url) else {
        return "v1/responses";
    };

    match url.path().trim_end_matches('/') {
        "/v1" | "/backend-api/codex" => "responses",
        _ => "v1/responses",
    }
}

fn previous_response_state_rejected(error: &ProviderError) -> bool {
    let ProviderError::Http { status, body } = error else {
        return false;
    };
    if !(*status == reqwest::StatusCode::BAD_REQUEST || *status == reqwest::StatusCode::NOT_FOUND) {
        return false;
    }

    let body = body.to_ascii_lowercase();
    body.contains("previous_response_id")
        || body.contains("previous response")
        || (body.contains("response") && body.contains("not found"))
        || (body.contains("response") && body.contains("expired"))
}

#[async_trait::async_trait]
impl<C> ProviderSession for ResponsesSession<C>
where
    C: CredentialSource + 'static,
{
    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        self.stream_response(request).await
    }

    async fn compact(
        &self,
        request: CompactionRequest<'_>,
    ) -> Result<CompactionResponse, ProviderError> {
        let request = request.into_model_request()?;
        let response = self.send_response(request).await?;
        Ok(response.into_compaction_response())
    }

    async fn summarize_memories(
        &self,
        request: MemorySummarizeRequest<'_>,
    ) -> Result<MemorySummarizeResponse, ProviderError> {
        let request = request.into_model_request()?;
        let response = self.send_response(request).await?;
        response.into_memory_summarize_response()
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::collections::BTreeMap;
    use std::io::Read;
    use std::io::Write;
    use std::net::TcpListener;
    use std::thread;

    use super::*;
    use crate::ProviderRequestOptions;
    use crate::StaticCredentialSource;
    use crate::responses::ResponsesProvider;

    fn spawn_single_response_server(
        response_body: &'static str,
    ) -> (String, thread::JoinHandle<String>) {
        spawn_single_response_server_with_headers(response_body, "")
    }

    fn spawn_single_response_server_with_headers(
        response_body: &'static str,
        extra_headers: &'static str,
    ) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("read listener addr");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut buffer = Vec::new();
            let mut temp = [0_u8; 1024];
            let mut header_end = None;
            let mut content_length = 0_usize;

            loop {
                let read = stream.read(&mut temp).expect("read request");
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&temp[..read]);
                if header_end.is_none()
                    && let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    let end = index + 4;
                    header_end = Some(end);
                    let headers = String::from_utf8_lossy(&buffer[..end]);
                    content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length").then(|| {
                                value.trim().parse::<usize>().expect("parse content-length")
                            })
                        })
                        .unwrap_or_default();
                }
                if let Some(end) = header_end
                    && buffer.len() >= end + content_length
                {
                    break;
                }
            }

            let response = format!(
                concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "content-type: text/event-stream\r\n",
                    "{}",
                    "content-length: {}\r\n\r\n",
                    "{}"
                ),
                extra_headers,
                response_body.len(),
                response_body,
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
            String::from_utf8(buffer).expect("request should be utf8")
        });

        (format!("http://{addr}/"), handle)
    }

    fn read_http_request(stream: &mut std::net::TcpStream) -> String {
        let mut buffer = Vec::new();
        let mut temp = [0_u8; 1024];
        let mut header_end = None;
        let mut content_length = 0_usize;

        loop {
            let read = stream.read(&mut temp).expect("read request");
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&temp[..read]);
            if header_end.is_none()
                && let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n")
            {
                let end = index + 4;
                header_end = Some(end);
                let headers = String::from_utf8_lossy(&buffer[..end]);
                content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().expect("parse content-length"))
                    })
                    .unwrap_or_default();
            }
            if let Some(end) = header_end
                && buffer.len() >= end + content_length
            {
                break;
            }
        }

        String::from_utf8(buffer).expect("request should be utf8")
    }

    fn request_body(captured: &str) -> &str {
        captured.split("\r\n\r\n").nth(1).unwrap_or_default()
    }

    fn spawn_hybrid_fallback_server() -> (String, thread::JoinHandle<(String, String)>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("read listener addr");
        let handle = thread::spawn(move || {
            let (mut first_stream, _) = listener.accept().expect("accept first request");
            let first = read_http_request(&mut first_stream);
            let first_body = r#"{"error":{"message":"previous_response_id not found"}}"#;
            let first_response = format!(
                concat!(
                    "HTTP/1.1 400 Bad Request\r\n",
                    "connection: close\r\n",
                    "content-type: application/json\r\n",
                    "content-length: {}\r\n\r\n",
                    "{}"
                ),
                first_body.len(),
                first_body
            );
            first_stream
                .write_all(first_response.as_bytes())
                .expect("write first response");
            drop(first_stream);

            let (mut second_stream, _) = listener.accept().expect("accept second request");
            let second = read_http_request(&mut second_stream);
            let second_body = concat!(
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_fresh\",\"model\":\"gpt-5\",\"status\":\"in_progress\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_fresh\",\"model\":\"gpt-5\",\"status\":\"completed\"}}\n\n"
            );
            let second_response = format!(
                concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "connection: close\r\n",
                    "content-type: text/event-stream\r\n",
                    "content-length: {}\r\n\r\n",
                    "{}"
                ),
                second_body.len(),
                second_body
            );
            second_stream
                .write_all(second_response.as_bytes())
                .expect("write second response");

            (first, second)
        });

        (format!("http://{addr}/"), handle)
    }

    fn spawn_two_turn_state_server() -> (String, thread::JoinHandle<(String, String)>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("read listener addr");
        let handle = thread::spawn(move || {
            let (mut first_stream, _) = listener.accept().expect("accept first request");
            let first = read_http_request(&mut first_stream);
            let first_body = concat!(
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5\",\"status\":\"in_progress\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5\",\"status\":\"completed\"}}\n\n"
            );
            let first_response = format!(
                concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "connection: close\r\n",
                    "content-type: text/event-stream\r\n",
                    "x-codex-turn-state: state-1\r\n",
                    "content-length: {}\r\n\r\n",
                    "{}"
                ),
                first_body.len(),
                first_body
            );
            first_stream
                .write_all(first_response.as_bytes())
                .expect("write first response");
            drop(first_stream);

            let (mut second_stream, _) = listener.accept().expect("accept second request");
            let second = read_http_request(&mut second_stream);
            let second_body = concat!(
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_2\",\"model\":\"gpt-5\",\"status\":\"in_progress\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_2\",\"model\":\"gpt-5\",\"status\":\"completed\"}}\n\n"
            );
            let second_response = format!(
                concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "connection: close\r\n",
                    "content-type: text/event-stream\r\n",
                    "x-codex-turn-state: state-2\r\n",
                    "content-length: {}\r\n\r\n",
                    "{}"
                ),
                second_body.len(),
                second_body
            );
            second_stream
                .write_all(second_response.as_bytes())
                .expect("write second response");

            (first, second)
        });

        (format!("http://{addr}/"), handle)
    }

    fn spawn_compaction_response_server(
        response_body: &'static str,
    ) -> (String, thread::JoinHandle<(String, String)>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("read listener addr");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut buffer = Vec::new();
            let mut temp = [0_u8; 1024];
            let mut header_end = None;
            let mut content_length = 0_usize;

            loop {
                let read = stream.read(&mut temp).expect("read request");
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&temp[..read]);
                if header_end.is_none()
                    && let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    let end = index + 4;
                    header_end = Some(end);
                    let headers = String::from_utf8_lossy(&buffer[..end]);
                    content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length").then(|| {
                                value.trim().parse::<usize>().expect("parse content-length")
                            })
                        })
                        .unwrap_or_default();
                }
                if let Some(end) = header_end
                    && buffer.len() >= end + content_length
                {
                    break;
                }
            }

            let response = format!(
                concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "content-type: text/event-stream\r\n",
                    "content-length: {}\r\n\r\n",
                    "{}"
                ),
                response_body.len(),
                response_body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
            let captured = String::from_utf8(buffer).expect("request should be utf8");
            let body = captured
                .split("\r\n\r\n")
                .nth(1)
                .unwrap_or_default()
                .to_string();
            (captured, body)
        });

        (format!("http://{addr}/"), handle)
    }

    #[tokio::test]
    async fn stream_response_honors_session_request_options_on_http_path() {
        let sse_body = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5\",\"status\":\"completed\"}}\n\n";
        let (base_url, handle) = spawn_single_response_server(sse_body);

        let mut definition = super::super::openai_definition();
        definition.base_url = Some(base_url);
        let session = ResponsesProvider::with_shared_credential_source(
            definition,
            Arc::new(StaticCredentialSource::new("test-key")),
        )
        .session();
        session.set_turn_state("sticky-turn-state");

        let request = Request {
            model: Cow::Borrowed("gpt-5"),
            system: Some(Cow::Borrowed("system")),
            messages: Cow::Owned(vec![crate::Message::user(crate::ContentBlock::text(
                "hello",
            ))]),
            tools: Cow::Owned(Vec::new()),
            tool_choice: None,
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
            provider_request_options: ProviderRequestOptions {
                session: SessionRequestOptions {
                    sticky_turn_state: None,
                    turn_metadata: Some("{\"turn_id\":\"turn-123\"}".to_string()),
                    subagent: Some("memory_consolidation".to_string()),
                    prefer_connection_reuse: Some(true),
                    session_affinity: Some("session-affinity-123".to_string()),
                    extra_headers: BTreeMap::new(),
                },
                ..ProviderRequestOptions::default()
            },
        };

        let _stream = session
            .stream_response(request)
            .await
            .expect("stream response should succeed");

        let captured = handle.join().expect("server should capture request");
        assert!(captured.contains("x-codex-turn-state: sticky-turn-state\r\n"));
        assert!(captured.contains("x-codex-turn-metadata: {\"turn_id\":\"turn-123\"}\r\n"));
        assert!(captured.contains("x-mentra-turn-metadata: {\"turn_id\":\"turn-123\"}\r\n"));
        assert!(captured.contains("x-mentra-session-affinity: session-affinity-123\r\n"));
        assert!(captured.contains("x-mentra-connection-reuse: prefer-reuse\r\n"));
        assert!(captured.contains("x-client-request-id: session-affinity-123\r\n"));
        assert!(captured.contains("session_id: session-affinity-123\r\n"));
        assert!(captured.contains("x-openai-subagent: memory_consolidation\r\n"));
    }

    #[tokio::test]
    async fn stream_response_captures_turn_state_from_http_response_headers() {
        let sse_body = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5\",\"status\":\"completed\"}}\n\n";
        let (base_url, _handle) = spawn_single_response_server_with_headers(
            sse_body,
            "x-codex-turn-state: next-turn-state\r\n",
        );

        let mut definition = super::super::openai_definition();
        definition.base_url = Some(base_url);
        let session = ResponsesProvider::with_shared_credential_source(
            definition,
            Arc::new(StaticCredentialSource::new("test-key")),
        )
        .session();

        let request = Request {
            model: Cow::Borrowed("gpt-5"),
            system: Some(Cow::Borrowed("system")),
            messages: Cow::Owned(vec![crate::Message::user(crate::ContentBlock::text(
                "hello",
            ))]),
            tools: Cow::Owned(Vec::new()),
            tool_choice: None,
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
            provider_request_options: ProviderRequestOptions::default(),
        };

        let _stream = session
            .stream_response(request)
            .await
            .expect("stream response should succeed");

        assert_eq!(session.turn_state().as_deref(), Some("next-turn-state"));
    }

    #[tokio::test]
    async fn http_turn_state_updates_across_multiple_turns() {
        let (base_url, handle) = spawn_two_turn_state_server();

        let mut definition = super::super::openai_definition();
        definition.base_url = Some(base_url);
        let session = ResponsesProvider::with_shared_credential_source(
            definition,
            Arc::new(StaticCredentialSource::new("test-key")),
        )
        .session();

        for message in ["first", "second"] {
            let request = Request {
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
            };

            let mut stream = session
                .stream_response(request)
                .await
                .expect("stream response should succeed");
            while let Some(event) = stream.recv().await {
                event.expect("stream event should decode");
            }
        }

        let (first, second) = handle.join().expect("server should capture requests");
        assert!(!first.contains("x-codex-turn-state:"));
        assert!(second.contains("x-codex-turn-state: state-1\r\n"));
        assert_eq!(session.turn_state().as_deref(), Some("state-2"));
        assert_eq!(session.latest_response_id().as_deref(), Some("resp_2"));
    }

    #[tokio::test]
    async fn stream_response_tracks_latest_response_id_from_http_events() {
        let sse_body = "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5\",\"status\":\"in_progress\"}}\n\n\
data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5\",\"status\":\"completed\"}}\n\n";
        let (base_url, _handle) = spawn_single_response_server(sse_body);

        let mut definition = super::super::openai_definition();
        definition.base_url = Some(base_url);
        let session = ResponsesProvider::with_shared_credential_source(
            definition,
            Arc::new(StaticCredentialSource::new("test-key")),
        )
        .session();

        let request = Request {
            model: Cow::Borrowed("gpt-5"),
            system: None,
            messages: Cow::Owned(vec![crate::Message::user(crate::ContentBlock::text(
                "hello",
            ))]),
            tools: Cow::Owned(Vec::new()),
            tool_choice: None,
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
            provider_request_options: ProviderRequestOptions::default(),
        };

        let mut stream = session
            .stream_response(request)
            .await
            .expect("stream response should succeed");
        while let Some(event) = stream.recv().await {
            event.expect("stream event should decode");
        }

        assert_eq!(session.latest_response_id().as_deref(), Some("resp_1"));
    }

    #[test]
    fn responses_endpoint_path_supports_openai_and_xipe_base_urls() {
        assert_eq!(
            responses_endpoint_path_for_base(Some("https://api.openai.com/")),
            "v1/responses"
        );
        assert_eq!(
            responses_endpoint_path_for_base(Some("https://api.openai.com/v1")),
            "responses"
        );
        assert_eq!(
            responses_endpoint_path_for_base(Some("https://chatgpt.com/backend-api/codex")),
            "responses"
        );
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)]
    async fn websocket_transport_sends_response_create_frame() {
        use futures_util::{SinkExt, StreamExt};
        use http::HeaderValue;
        use tokio_tungstenite::accept_hdr_async;
        use tokio_tungstenite::tungstenite::Message as WsMessage;
        use tokio_tungstenite::tungstenite::handshake::server::{
            Request as WsHandshakeRequest, Response as WsHandshakeResponse,
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind websocket test server");
        let addr = listener.local_addr().expect("read websocket server addr");
        let (tx_frame, rx_frame) = tokio::sync::oneshot::channel::<String>();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept websocket");
            let mut ws = accept_hdr_async(
                stream,
                |_request: &WsHandshakeRequest, mut response: WsHandshakeResponse| {
                    response
                        .headers_mut()
                        .insert("x-codex-turn-state", HeaderValue::from_static("ws-state"));
                    Ok(response)
                },
            )
            .await
            .expect("upgrade websocket");
            let frame = ws
                .next()
                .await
                .expect("client should send a frame")
                .expect("websocket frame should be valid")
                .into_text()
                .expect("request frame should be text")
                .to_string();
            tx_frame.send(frame).expect("send captured frame");

            ws.send(WsMessage::Text(
                serde_json::json!({
                    "type": "response.created",
                    "response": {
                        "id": "resp_ws",
                        "model": "gpt-5",
                        "status": "in_progress"
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send response.created");
            ws.send(WsMessage::Text(
                serde_json::json!({
                    "type": "response.completed",
                    "response": {
                        "id": "resp_ws",
                        "model": "gpt-5",
                        "status": "completed"
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send response.completed");
        });

        let mut definition = super::super::openai_definition();
        definition.base_url = Some(format!("http://{addr}/v1"));
        let session = ResponsesProvider::with_shared_credential_source(
            definition,
            Arc::new(StaticCredentialSource::new("test-key")),
        )
        .session();

        let request = Request {
            model: Cow::Borrowed("gpt-5"),
            system: None,
            messages: Cow::Owned(vec![crate::Message::user(crate::ContentBlock::text(
                "hello",
            ))]),
            tools: Cow::Owned(Vec::new()),
            tool_choice: None,
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
            provider_request_options: ProviderRequestOptions {
                responses: crate::ResponsesRequestOptions {
                    transport: crate::ResponsesTransport::WebSocket,
                    ..Default::default()
                },
                ..ProviderRequestOptions::default()
            },
        };

        let mut stream = session
            .stream_response(request)
            .await
            .expect("websocket transport should stream");
        while let Some(event) = stream.recv().await {
            event.expect("websocket event should parse");
        }

        let frame: serde_json::Value =
            serde_json::from_str(&rx_frame.await.expect("server should capture frame"))
                .expect("frame should be json");
        assert_eq!(frame["type"], "response.create");
        assert_eq!(frame["response"]["model"], "gpt-5");
        assert_eq!(frame["response"]["input"][0]["role"], "user");
        assert_eq!(session.turn_state().as_deref(), Some("ws-state"));
        assert_eq!(session.latest_response_id().as_deref(), Some("resp_ws"));
    }

    #[tokio::test]
    async fn hybrid_state_falls_back_to_replay_when_previous_response_id_is_rejected() {
        let (base_url, handle) = spawn_hybrid_fallback_server();

        let mut definition = super::super::openai_definition();
        definition.base_url = Some(base_url);
        let session = ResponsesProvider::with_shared_credential_source(
            definition,
            Arc::new(StaticCredentialSource::new("test-key")),
        )
        .session();
        session.state.set_latest_response_id("resp_stale");

        let request = Request {
            model: Cow::Borrowed("gpt-5"),
            system: None,
            messages: Cow::Owned(vec![crate::Message::user(crate::ContentBlock::text(
                "hello",
            ))]),
            tools: Cow::Owned(Vec::new()),
            tool_choice: None,
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
            provider_request_options: ProviderRequestOptions::default(),
        };

        let mut stream = session
            .stream_response(request)
            .await
            .expect("hybrid fallback should retry without provider state");
        while let Some(event) = stream.recv().await {
            event.expect("stream event should decode");
        }

        let (first, second) = handle.join().expect("server should capture requests");
        let first_payload: serde_json::Value =
            serde_json::from_str(request_body(&first)).expect("first body should be json");
        let second_payload: serde_json::Value =
            serde_json::from_str(request_body(&second)).expect("second body should be json");
        assert_eq!(first_payload["previous_response_id"], "resp_stale");
        assert!(second_payload.get("previous_response_id").is_none());
        assert_eq!(session.latest_response_id().as_deref(), Some("resp_fresh"));
    }

    #[tokio::test]
    async fn compact_sends_normal_model_request_and_wraps_summary_text() {
        let sse_body = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5\",\"status\":\"in_progress\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"content\":[]}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"{\\\"goal\\\":\\\"keep going\\\"}\"}]}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5\",\"status\":\"completed\"}}\n\n"
        );
        let (base_url, handle) = spawn_compaction_response_server(sse_body);

        let mut definition = super::super::openai_definition();
        definition.base_url = Some(base_url);
        let session = ResponsesProvider::with_shared_credential_source(
            definition,
            Arc::new(StaticCredentialSource::new("test-key")),
        )
        .session();

        let request = crate::CompactionRequest {
            model: Cow::Borrowed("gpt-5"),
            instructions: Cow::Borrowed("Summarize the transcript."),
            input: Cow::Owned(vec![crate::CompactionInputItem::UserTurn {
                content: "hello".to_string(),
            }]),
            metadata: Cow::Owned(BTreeMap::from([("scope".to_string(), "test".to_string())])),
            provider_request_options: crate::ProviderRequestOptions::default(),
        };

        let response = session.compact(request).await.expect("compaction succeeds");
        let captured = handle.join().expect("server should capture request");

        let payload: serde_json::Value =
            serde_json::from_str(&captured.1).expect("request body should be json");
        assert_eq!(payload["model"], "gpt-5");
        assert_eq!(payload["instructions"], "Summarize the transcript.");
        assert_eq!(payload["metadata"]["scope"], "test");
        assert_eq!(payload["input"][0]["content"][0]["type"], "input_text");
        assert!(
            payload["input"][0]["content"][0]["text"]
                .as_str()
                .expect("prompt text should be a string")
                .starts_with("Compaction input JSON:\n")
        );

        assert_eq!(response.output.len(), 1);
        assert_eq!(
            response.output[0],
            crate::CompactionInputItem::CompactionSummary {
                content: "{\"goal\":\"keep going\"}".to_string()
            }
        );
    }

    #[tokio::test]
    async fn summarize_memories_sends_normal_model_request_and_parses_json_output() {
        let sse_body = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_2\",\"model\":\"gpt-5\",\"status\":\"in_progress\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"content\":[]}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"[{\\\"raw_memory\\\":\\\"Detailed summary\\\",\\\"memory_summary\\\":\\\"Short summary\\\"}]\"}]}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_2\",\"model\":\"gpt-5\",\"status\":\"completed\"}}\n\n"
        );
        let (base_url, handle) = spawn_compaction_response_server(sse_body);

        let mut definition = super::super::openai_definition();
        definition.base_url = Some(base_url);
        let session = ResponsesProvider::with_shared_credential_source(
            definition,
            Arc::new(StaticCredentialSource::new("test-key")),
        )
        .session();

        let request = crate::MemorySummarizeRequest {
            model: Cow::Borrowed("gpt-5"),
            raw_memories: Cow::Owned(vec![crate::RawMemory {
                id: "memory-1".to_string(),
                metadata: crate::RawMemoryMetadata {
                    source_path: "/tmp/trace.jsonl".to_string(),
                },
                items: vec![serde_json::json!({"type":"message","role":"user"})],
            }]),
            reasoning: Some(crate::ReasoningOptions {
                effort: Some(crate::ReasoningEffort::Medium),
                summary: None,
            }),
            metadata: Cow::Owned(BTreeMap::from([("scope".to_string(), "test".to_string())])),
            provider_request_options: crate::ProviderRequestOptions {
                session: crate::SessionRequestOptions {
                    sticky_turn_state: None,
                    turn_metadata: Some("{\"turn_id\":\"turn-321\"}".to_string()),
                    subagent: Some("compact".to_string()),
                    prefer_connection_reuse: Some(true),
                    session_affinity: Some("session-affinity-321".to_string()),
                    extra_headers: BTreeMap::new(),
                },
                ..crate::ProviderRequestOptions::default()
            },
        };

        let response = session
            .summarize_memories(request)
            .await
            .expect("memory summarization succeeds");
        let captured = handle.join().expect("server should capture request");

        let payload: serde_json::Value =
            serde_json::from_str(&captured.1).expect("request body should be json");
        assert_eq!(payload["model"], "gpt-5");
        assert_eq!(payload["reasoning"]["effort"], "medium");
        assert_eq!(payload["metadata"]["scope"], "test");
        assert_eq!(payload["input"][0]["content"][0]["type"], "input_text");
        assert!(
            payload["input"][0]["content"][0]["text"]
                .as_str()
                .expect("prompt text should be a string")
                .starts_with("Memory summarize input JSON:\n")
        );

        assert_eq!(response.output.len(), 1);
        assert_eq!(response.output[0].raw_memory, "Detailed summary");
        assert_eq!(response.output[0].memory_summary, "Short summary");
        assert!(
            captured
                .0
                .contains("x-codex-turn-metadata: {\"turn_id\":\"turn-321\"}\r\n")
        );
        assert!(
            captured
                .0
                .contains("x-mentra-session-affinity: session-affinity-321\r\n")
        );
        assert!(
            captured
                .0
                .contains("x-client-request-id: session-affinity-321\r\n")
        );
        assert!(captured.0.contains("session_id: session-affinity-321\r\n"));
        assert!(captured.0.contains("x-openai-subagent: compact\r\n"));
    }

    #[tokio::test]
    async fn websocket_connection_is_closed_without_cached_connection() {
        let session = ResponsesProvider::with_shared_credential_source(
            super::super::openai_definition(),
            Arc::new(StaticCredentialSource::new("test-key")),
        )
        .session();

        assert!(session.websocket_connection_is_closed().await);
        session.clear_websocket_connection();
        assert!(session.websocket_connection_is_closed().await);
    }
}
