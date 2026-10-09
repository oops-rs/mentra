//! The A2A client: one remote agent, reached over the JSON-RPC binding.
//!
//! # Lifecycle
//!
//! 1. [`connect`](A2aClient::connect) fetches the Agent Card from the
//!    configured URL and picks the first `supportedInterfaces` entry whose
//!    `protocolBinding` is `JSONRPC` and whose `protocolVersion` is 1.0 (any
//!    patch level). A card with no such entry is refused rather than spoken
//!    to in a binding or version this client does not implement.
//! 2. Every method is then one JSON-RPC `POST` to that interface's URL,
//!    carrying `A2A-Version: 1.0` and the configured headers, answered in the
//!    same response. There is no session and no background task, so a client
//!    holds nothing that needs shutting down.
//!
//! # Failure behavior
//!
//! Nothing is retried and no redirect is followed. A `SendMessage` whose
//! request may have reached the agent but whose answer never arrived is
//! reported as [`A2aError::SendIndeterminate`] rather than as a plain failure:
//! the agent may already be working on it, and resending would start a second
//! task. `GetTask` and `CancelTask` are safe to repeat and are reported as
//! what went wrong.

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicI64, Ordering};

use a2a_lf::{
    AgentCard, AgentInterface, CancelTaskRequest, GetTaskRequest, JsonRpcId, JsonRpcRequest,
    JsonRpcResponse, Message, SendMessageConfiguration, SendMessageRequest, SendMessageResponse,
    TRANSPORT_PROTOCOL_JSONRPC, Task, methods,
};
use futures_util::StreamExt;
use reqwest::StatusCode;
use reqwest::header::{ACCEPT, HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;
use serde::de::DeserializeOwned;
use url::Url;

use super::config::{A2aAgentConfig, A2aConfigError, A2aLimits, check_url};
use super::error::{A2aError, A2aRpcError};

/// The protocol version this client speaks, sent as `A2A-Version`.
///
/// The specification reads a request without the header as version 0.3, and
/// the reference server refuses it, so it is on every request — the card
/// fetch included.
pub const A2A_PROTOCOL_VERSION: &str = "1.0";

/// The header naming the protocol version.
const VERSION_HEADER: &str = "a2a-version";

/// A connected remote agent.
///
/// This is the low-level client: it speaks the protocol and nothing else.
/// [`A2aAgentTool`](super::A2aAgentTool) builds the polling tool a model calls
/// on top of it; a host that wants its own policy over the top can use this
/// directly.
pub struct A2aClient {
    name: String,
    http: reqwest::Client,
    card_url: Url,
    card: AgentCard,
    /// The JSON-RPC interface the card offered and this client chose.
    endpoint: Url,
    /// The tenant that interface names, sent on every request when present.
    tenant: Option<String>,
    headers: HeaderMap,
    limits: A2aLimits,
    next_id: AtomicI64,
}

impl A2aClient {
    /// Fetches and validates the agent's card and selects its JSON-RPC 1.0
    /// interface.
    pub async fn connect(config: &A2aAgentConfig) -> Result<Self, A2aError> {
        let card_url = config.validate()?;
        let headers = build_headers(config)?;
        let http = build_http_client(&config.limits)?;

        let request = http
            .get(card_url.clone())
            .headers(headers.clone())
            .header(ACCEPT, "application/json");
        let (status, body) = exchange(request, &config.limits).await?;
        if !status.is_success() {
            return Err(A2aError::HttpStatus {
                operation: "the agent card request",
                status,
            });
        }
        let card: AgentCard = serde_json::from_slice(&body)
            .map_err(|_| A2aError::InvalidCard("it is not a JSON Agent Card"))?;
        if card.name.trim().is_empty() {
            return Err(A2aError::InvalidCard("it has no name"));
        }

        let interface = select_interface(&card).ok_or(A2aError::NoCompatibleInterface)?;
        let endpoint = check_url(&interface.url).map_err(A2aError::InvalidEndpoint)?;
        if config.refuses_plaintext_to(&endpoint) {
            return Err(A2aError::PlaintextEndpoint);
        }
        let tenant = interface.tenant.clone().filter(|tenant| !tenant.is_empty());

        Ok(Self {
            name: config.name.clone(),
            http,
            card_url,
            card,
            endpoint,
            tenant,
            headers,
            limits: config.limits.clone(),
            next_id: AtomicI64::new(1),
        })
    }

    /// The configured name of this agent, which names its tool.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The Agent Card fetched at connect time.
    pub fn card(&self) -> &AgentCard {
        &self.card
    }

    /// The URL the card was fetched from.
    pub fn card_url(&self) -> &Url {
        &self.card_url
    }

    /// The JSON-RPC endpoint every call is posted to.
    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// The timeouts and limits this client was configured with.
    pub fn limits(&self) -> &A2aLimits {
        &self.limits
    }

    /// Sends a message, starting a task or continuing one.
    ///
    /// `configuration.return_immediately` decides whether the agent answers
    /// as soon as the task exists or only once it settles. The agent may
    /// answer with a [`Message`] instead of a task when the exchange needs no
    /// task at all; both arrive as [`SendMessageResponse`].
    ///
    /// A message that names a task must be answered about that task; an answer
    /// about another one is refused as [`A2aError::MismatchedTask`].
    pub async fn send_message(
        &self,
        message: Message,
        configuration: SendMessageConfiguration,
    ) -> Result<SendMessageResponse, A2aError> {
        let task_id = message.task_id.clone();
        let request = SendMessageRequest {
            message,
            configuration: Some(configuration),
            metadata: None,
            tenant: self.tenant.clone(),
        };
        let response: SendMessageResponse = self
            .call(methods::SEND_MESSAGE, &request)
            .await
            .map_err(send_failure)?;

        if let (Some(expected), SendMessageResponse::Task(task)) = (&task_id, &response)
            && task.id != *expected
        {
            return Err(A2aError::MismatchedTask {
                method: methods::SEND_MESSAGE,
            });
        }
        Ok(response)
    }

    /// Reads a task's current state.
    ///
    /// `history_length` bounds how many history messages come back; `Some(0)`
    /// asks for none, and `None` leaves it to the agent.
    pub async fn get_task(
        &self,
        task_id: &str,
        history_length: Option<i32>,
    ) -> Result<Task, A2aError> {
        let request = GetTaskRequest {
            id: task_id.to_string(),
            history_length,
            tenant: self.tenant.clone(),
        };
        let task: Task = self.call(methods::GET_TASK, &request).await?;
        expect_task(methods::GET_TASK, task_id, task)
    }

    /// Asks the agent to cancel a task, returning the task as it then stands.
    pub async fn cancel_task(&self, task_id: &str) -> Result<Task, A2aError> {
        let request = CancelTaskRequest {
            id: task_id.to_string(),
            metadata: None,
            tenant: self.tenant.clone(),
        };
        let task: Task = self.call(methods::CANCEL_TASK, &request).await?;
        expect_task(methods::CANCEL_TASK, task_id, task)
    }

    /// Posts one JSON-RPC request and decodes the result it answered with.
    async fn call<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &'static str,
        params: &P,
    ) -> Result<R, A2aError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let params = serde_json::to_value(params).map_err(|_| A2aError::Decode { method })?;
        let request = JsonRpcRequest::new(JsonRpcId::Number(id), method, Some(params));

        let builder = self
            .http
            .post(self.endpoint.clone())
            .headers(self.headers.clone())
            .header(ACCEPT, "application/json")
            .json(&request);
        let (status, body) = exchange(builder, &self.limits).await?;

        let Ok(reply) = serde_json::from_slice::<JsonRpcResponse>(&body) else {
            // A gateway in front of the agent answers in its own terms; only a
            // body that is not JSON-RPC at all is reported by its status.
            if !status.is_success() {
                return Err(A2aError::HttpStatus {
                    operation: method,
                    status,
                });
            }
            return Err(A2aError::Decode { method });
        };

        if let Some(error) = reply.error {
            // A request the server could not parse is answered with a null id,
            // and that answer is still about this request.
            if reply.id != JsonRpcId::Number(id) && reply.id != JsonRpcId::Null {
                return Err(A2aError::MismatchedReplyId { method });
            }
            return Err(A2aError::Rpc {
                method,
                error: A2aRpcError::from_code(error.code),
            });
        }
        if reply.id != JsonRpcId::Number(id) {
            return Err(A2aError::MismatchedReplyId { method });
        }
        if !status.is_success() {
            return Err(A2aError::HttpStatus {
                operation: method,
                status,
            });
        }

        let result = reply.result.ok_or(A2aError::Decode { method })?;
        serde_json::from_value(result).map_err(|_| A2aError::Decode { method })
    }
}

impl std::fmt::Debug for A2aClient {
    /// Renders without the header map, which holds credentials.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A2aClient")
            .field("name", &self.name)
            .field("card_url", &self.card_url.as_str())
            .field("endpoint", &self.endpoint.as_str())
            .finish_non_exhaustive()
    }
}

/// The first interface this client can speak: JSON-RPC at protocol 1.0.
///
/// The card lists interfaces in the agent's order of preference, so the first
/// match is the one the agent would choose.
fn select_interface(card: &AgentCard) -> Option<&AgentInterface> {
    card.supported_interfaces.iter().find(|interface| {
        interface
            .protocol_binding
            .eq_ignore_ascii_case(TRANSPORT_PROTOCOL_JSONRPC)
            && speaks_version(&interface.protocol_version)
    })
}

/// Whether a declared protocol version is 1.0, at any patch level.
///
/// A2A versions are `Major.Minor`; a minor version may add behavior a 1.0
/// client does not implement, so only 1.0 itself is accepted. A bare `1` is
/// read as 1.0.
fn speaks_version(declared: &str) -> bool {
    let mut parts = declared.trim().split('.');
    let major = parts.next().and_then(|part| part.parse::<u32>().ok());
    let minor = match parts.next() {
        None => Some(0),
        Some(part) => part.parse::<u32>().ok(),
    };
    major == Some(1) && minor == Some(0)
}

/// Refuses a task that is not the one asked about.
fn expect_task(method: &'static str, task_id: &str, task: Task) -> Result<Task, A2aError> {
    if task.id != task_id {
        return Err(A2aError::MismatchedTask { method });
    }
    Ok(task)
}

/// Converts a `SendMessage` failure into the certainty a caller needs.
///
/// Failures that prove the agent never accepted the message are reported as
/// themselves: a connection that was never made, a 4xx (the agent refusing
/// the request before acting on it), a JSON-RPC error (the agent's answer),
/// and a local configuration error. Everything else happened after the
/// request may have been accepted — a timeout, a dropped connection, a 5xx, a
/// redirect, a reply that does not decode — and is indeterminate.
fn send_failure(error: A2aError) -> A2aError {
    match &error {
        A2aError::HttpStatus { status, .. } if status.is_client_error() => error,
        A2aError::Connect | A2aError::Config(_) | A2aError::Rpc { .. } => error,
        _ => A2aError::SendIndeterminate,
    }
}

/// Sends one request and reads the whole reply, bounded in time and size.
///
/// One deadline covers the request and the body, so an agent cannot evade
/// `request_timeout` by sending the head and withholding the body.
async fn exchange(
    request: reqwest::RequestBuilder,
    limits: &A2aLimits,
) -> Result<(StatusCode, Vec<u8>), A2aError> {
    let operation = async {
        let response = request.send().await.map_err(transport_error)?;
        let status = response.status();
        if status.is_redirection() {
            return Err(A2aError::RedirectRefused);
        }
        let body = read_bounded(response, limits.max_response_bytes).await?;
        Ok((status, body))
    };
    tokio::time::timeout(limits.request_timeout, operation)
        .await
        .map_err(|_| A2aError::Timeout(limits.request_timeout))?
}

/// Reads a response body, refusing one larger than `limit`.
async fn read_bounded(response: reqwest::Response, limit: usize) -> Result<Vec<u8>, A2aError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(A2aError::ResponseTooLarge { limit });
    }

    let mut body = response.bytes_stream();
    let mut buffer = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|_| A2aError::Transport("the reply body could not be read"))?;
        if buffer.len().saturating_add(chunk.len()) > limit {
            return Err(A2aError::ResponseTooLarge { limit });
        }
        buffer.extend_from_slice(&chunk);
    }
    Ok(buffer)
}

/// Builds the HTTP client used for every request to the agent.
fn build_http_client(limits: &A2aLimits) -> Result<reqwest::Client, A2aError> {
    reqwest::Client::builder()
        // Redirects are never followed. reqwest strips `Authorization` only
        // across hosts and compares host and port without scheme, so an
        // https->http redirect on the same host would keep the credential and
        // send it in the clear. A followed redirect would also hand the message
        // body to the new target.
        .redirect(reqwest::redirect::Policy::none())
        // A SendMessage may already have started a task by the time a
        // protocol-level rejection arrives, so reqwest's own resend is off.
        .retry(reqwest::retry::never())
        .connect_timeout(limits.connect_timeout)
        .build()
        .map_err(|_| A2aError::Transport("the HTTP client could not be built"))
}

/// Converts configured headers into a map, marking every value sensitive, and
/// adds the protocol version.
fn build_headers(config: &A2aAgentConfig) -> Result<HeaderMap, A2aError> {
    let mut headers = HeaderMap::new();

    for (name, value) in &config.headers {
        let header_name = HeaderName::try_from(name.as_str())
            .map_err(|_| A2aConfigError::InvalidHeaderName { name: name.clone() })?;
        let mut header_value = HeaderValue::try_from(value.expose_secret())
            .map_err(|_| A2aConfigError::InvalidHeaderValue { name: name.clone() })?;
        // A plain HeaderValue prints its contents in Debug output. Marking it
        // sensitive redacts it there and tells HTTP/2 not to index it.
        header_value.set_sensitive(true);
        headers.insert(header_name, header_value);
    }

    headers.insert(
        HeaderName::from_static(VERSION_HEADER),
        HeaderValue::from_static(A2A_PROTOCOL_VERSION),
    );
    Ok(headers)
}

/// Renders a transport failure without echoing anything the server sent.
fn transport_error(error: reqwest::Error) -> A2aError {
    if error.is_connect() {
        A2aError::Connect
    } else if error.is_timeout() {
        A2aError::Transport("the request timed out")
    } else if error.is_request() {
        A2aError::Transport("the request could not be sent")
    } else {
        A2aError::Transport("the connection failed")
    }
}
