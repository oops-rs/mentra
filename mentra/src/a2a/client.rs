//! The A2A client: one remote agent, reached over the JSON-RPC or the
//! HTTP+JSON binding.
//!
//! # Lifecycle
//!
//! 1. [`connect`](A2aClient::connect) fetches the Agent Card from the
//!    configured URL and picks the first `supportedInterfaces` entry, in the
//!    card's own order, whose `protocolBinding` is `JSONRPC` or `HTTP+JSON`
//!    and whose `protocolVersion` is 1.x. A card with no such entry is refused
//!    rather than spoken to in a binding or version this client does not
//!    implement.
//! 2. Every method is then one HTTP request to that interface — a JSON-RPC
//!    `POST`, or the HTTP+JSON verb and path — carrying `A2A-Version: 1.0`,
//!    the configured headers, and the interface's `tenant` when it names one,
//!    answered in the same response. There is no session and no background
//!    task, so a client holds nothing that needs shutting down.
//!
//! # Failure behavior
//!
//! Nothing is retried and no redirect is followed. A `SendMessage` whose
//! request may have reached the agent but whose answer never arrived is
//! reported as [`A2aError::SendIndeterminate`] rather than as a plain failure:
//! the agent may already be working on it, and resending would start a second
//! task. `GetTask` and `CancelTask` are safe to repeat and are reported as
//! what went wrong.

mod binding;
mod http;
mod jsonrpc;
mod rest;
mod sse;

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicI64, Ordering};

use a2a_lf::{
    AgentCard, CancelTaskRequest, GetTaskRequest, Message, SendMessageConfiguration,
    SendMessageRequest, SendMessageResponse, SubscribeToTaskRequest, Task, methods,
};
use reqwest::Method;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap};
use serde::de::DeserializeOwned;
use serde_json::Value;
use url::Url;

use super::config::{A2aAgentConfig, A2aLimits, check_url};
use super::error::A2aError;
use binding::{Operation, select_interface};
use sse::{Decoder, EVENT_STREAM, EventFormat};

pub use binding::A2aBinding;
pub use sse::A2aEventStream;

/// The protocol version this client speaks, sent as `A2A-Version`.
///
/// The specification reads a request without the header as version 0.3, and
/// the reference server refuses it, so it is on every request — the card
/// fetch included.
pub const A2A_PROTOCOL_VERSION: &str = "1.0";

/// The media type of a reply that is one JSON document.
const JSON: &str = "application/json";

/// A connected remote agent.
///
/// This is the low-level client: it speaks the protocol and nothing else.
/// [`A2aAgentTool`](super::A2aAgentTool) builds the polling tool a model calls
/// on top of it; a host that wants its own policy over the top can use this
/// directly.
pub struct A2aClient {
    name: String,
    card_url: Url,
    card: AgentCard,
    transport: Transport,
}

/// How requests reach the agent: one interface, in one binding.
struct Transport {
    http: reqwest::Client,
    binding: A2aBinding,
    /// The URL of the interface the card offered and this client chose.
    endpoint: Url,
    /// The tenant that interface names, sent on every request when present.
    tenant: Option<String>,
    headers: HeaderMap,
    limits: A2aLimits,
    next_id: AtomicI64,
}

impl A2aClient {
    /// Fetches and validates the agent's card and selects the first interface
    /// it offers in a binding and version this client speaks.
    pub async fn connect(config: &A2aAgentConfig) -> Result<Self, A2aError> {
        let card_url = config.validate()?;
        let headers = http::build_headers(config)?;
        let http = http::build_http_client(&config.limits)?;

        let request = http
            .get(card_url.clone())
            .headers(headers.clone())
            .header(ACCEPT, JSON);
        let (status, body) = http::exchange(request, &config.limits).await?;
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

        let (binding, interface) =
            select_interface(&card).ok_or(A2aError::NoCompatibleInterface)?;
        let endpoint = check_url(&interface.url).map_err(A2aError::InvalidEndpoint)?;
        if config.refuses_plaintext_to(&endpoint) {
            return Err(A2aError::PlaintextEndpoint);
        }
        let tenant = interface.tenant.clone().filter(|tenant| !tenant.is_empty());

        Ok(Self {
            name: config.name.clone(),
            card_url,
            card,
            transport: Transport {
                http,
                binding,
                endpoint,
                tenant,
                headers,
                limits: config.limits.clone(),
                next_id: AtomicI64::new(1),
            },
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

    /// The binding of the interface this client chose.
    pub fn binding(&self) -> A2aBinding {
        self.transport.binding
    }

    /// The URL of the interface this client chose: the JSON-RPC endpoint every
    /// call is posted to, or the base every HTTP+JSON path is under.
    pub fn endpoint(&self) -> &Url {
        &self.transport.endpoint
    }

    /// The tenant the chosen interface names, sent on every request.
    pub fn tenant(&self) -> Option<&str> {
        self.transport.tenant.as_deref()
    }

    /// The timeouts and limits this client was configured with.
    pub fn limits(&self) -> &A2aLimits {
        &self.transport.limits
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
            tenant: self.transport.tenant.clone(),
        };
        let operation = Operation::new(
            methods::SEND_MESSAGE,
            &request,
            Method::POST,
            segments(["message:send"]),
        )?;
        let response: SendMessageResponse =
            self.transport.call(operation).await.map_err(send_failure)?;

        if let (Some(expected), SendMessageResponse::Task(task)) = (&task_id, &response)
            && task.id != *expected
        {
            return Err(A2aError::MismatchedTask {
                method: methods::SEND_MESSAGE,
            });
        }
        Ok(response)
    }

    /// Sends a message and streams what follows: the task (or a direct reply
    /// message) first, then its status and artifact updates, until the agent
    /// closes the stream.
    ///
    /// Opening the stream fails as [`send_message`](Self::send_message) does,
    /// [`A2aError::SendIndeterminate`] included. Once it is open the task
    /// exists, so an error mid-stream says nothing about whether to resend:
    /// the task's id is in the first event, and
    /// [`subscribe_to_task`](Self::subscribe_to_task) or
    /// [`get_task`](Self::get_task) pick it up again.
    pub async fn send_streaming_message(
        &self,
        message: Message,
        configuration: SendMessageConfiguration,
    ) -> Result<A2aEventStream, A2aError> {
        let task_id = message.task_id.clone();
        let request = SendMessageRequest {
            message,
            configuration: Some(configuration),
            metadata: None,
            tenant: self.transport.tenant.clone(),
        };
        let operation = Operation::new(
            methods::SEND_STREAMING_MESSAGE,
            &request,
            Method::POST,
            segments(["message:stream"]),
        )?;
        self.transport
            .open_stream(operation, task_id)
            .await
            .map_err(send_failure)
    }

    /// Streams a task's updates from now until it ends: the task as it stands
    /// first, then its status and artifact updates.
    ///
    /// The agent refuses a task that has already reached a terminal state, with
    /// `UnsupportedOperationError`; [`get_task`](Self::get_task) reads one.
    pub async fn subscribe_to_task(&self, task_id: &str) -> Result<A2aEventStream, A2aError> {
        let request = SubscribeToTaskRequest {
            id: task_id.to_string(),
            tenant: self.transport.tenant.clone(),
        };
        // `GET`, as the normative `a2a.proto` binds it; the specification's
        // prose table says `POST`, and the reference server accepts both.
        let operation = Operation::new(
            methods::SUBSCRIBE_TO_TASK,
            &request,
            Method::GET,
            segments(["tasks", &format!("{task_id}:subscribe")]),
        )?;
        self.transport
            .open_stream(operation, Some(task_id.to_string()))
            .await
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
            tenant: self.transport.tenant.clone(),
        };
        let operation = Operation::new(
            methods::GET_TASK,
            &request,
            Method::GET,
            segments(["tasks", task_id]),
        )?
        .query("historyLength", history_length);
        let task: Task = self.transport.call(operation).await?;
        expect_task(methods::GET_TASK, task_id, task)
    }

    /// Asks the agent to cancel a task, returning the task as it then stands.
    pub async fn cancel_task(&self, task_id: &str) -> Result<Task, A2aError> {
        let request = CancelTaskRequest {
            id: task_id.to_string(),
            metadata: None,
            tenant: self.transport.tenant.clone(),
        };
        let operation = Operation::new(
            methods::CANCEL_TASK,
            &request,
            Method::POST,
            segments(["tasks", &format!("{task_id}:cancel")]),
        )?;
        let task: Task = self.transport.call(operation).await?;
        expect_task(methods::CANCEL_TASK, task_id, task)
    }
}

impl Transport {
    /// Sends one operation in this client's binding and decodes what it
    /// answered with.
    async fn call<R: DeserializeOwned>(&self, operation: Operation) -> Result<R, A2aError> {
        let method = operation.method;
        let value = self.call_value(operation).await?;
        serde_json::from_value(value).map_err(|_| A2aError::Decode { method })
    }

    /// Sends one operation and returns the JSON it answered with, `Null` when
    /// it answered with none.
    async fn call_value(&self, operation: Operation) -> Result<Value, A2aError> {
        match self.binding {
            A2aBinding::JsonRpc => {
                let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                let request = self.jsonrpc_request(&operation, id, JSON);
                let (status, body) = http::exchange(request, &self.limits).await?;
                jsonrpc::decode_reply(operation.method, id, status, &body)
            }
            A2aBinding::HttpJson => {
                let request = self.rest_request(&operation, JSON)?;
                let (status, body) = http::exchange(request, &self.limits).await?;
                rest::decode_reply(operation.method, status, &body)
            }
        }
    }
}

impl Transport {
    /// Sends one streaming operation and returns its events once the reply's
    /// head has arrived.
    ///
    /// A reply that is not an event stream is read whole, as a plain reply
    /// would be, so an error the agent answered with is reported as itself.
    async fn open_stream(
        &self,
        operation: Operation,
        expected_task: Option<String>,
    ) -> Result<A2aEventStream, A2aError> {
        let method = operation.method;
        let (request, format) = match self.binding {
            A2aBinding::JsonRpc => {
                let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                let request = self.jsonrpc_request(&operation, id, EVENT_STREAM);
                (request, EventFormat::JsonRpc { id })
            }
            A2aBinding::HttpJson => (
                self.rest_request(&operation, EVENT_STREAM)?,
                EventFormat::HttpJson,
            ),
        };

        // One deadline covers the head and, for a reply that is not a
        // stream, its body; a stream's own events are bounded by the idle
        // timeout instead.
        let timeout = self.limits.request_timeout;
        let deadline = tokio::time::Instant::now() + timeout;
        let response = tokio::time::timeout_at(deadline, http::send(request))
            .await
            .map_err(|_| A2aError::Timeout(timeout))??;
        let status = response.status();
        if status.is_success() && is_event_stream(&response) {
            let decoder = Decoder { method, format };
            return Ok(A2aEventStream::new(
                response,
                decoder,
                expected_task,
                &self.limits,
            ));
        }

        let read = http::read_bounded(response, self.limits.max_response_bytes);
        let body = tokio::time::timeout_at(deadline, read)
            .await
            .map_err(|_| A2aError::Timeout(timeout))??;
        match format {
            EventFormat::JsonRpc { id } => jsonrpc::decode_reply(method, id, status, &body),
            EventFormat::HttpJson => rest::decode_reply(method, status, &body),
        }?;
        Err(A2aError::NotAStream { method })
    }
}

impl std::fmt::Debug for A2aClient {
    /// Renders without the header map, which holds credentials.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A2aClient")
            .field("name", &self.name)
            .field("card_url", &self.card_url.as_str())
            .field("binding", &self.transport.binding)
            .field("endpoint", &self.transport.endpoint.as_str())
            .finish_non_exhaustive()
    }
}

/// Whether a reply declares itself an event stream.
fn is_event_stream(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .trim_start()
                .get(..EVENT_STREAM.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(EVENT_STREAM))
        })
}

/// An HTTP+JSON path, as owned segments.
fn segments<const N: usize>(parts: [&str; N]) -> Vec<String> {
    parts.into_iter().map(str::to_string).collect()
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
/// the request before acting on it), an A2A error (the agent's answer), and a
/// local configuration error. Everything else happened after the request may
/// have been accepted — a timeout, a dropped connection, a 5xx, a redirect, a
/// reply that does not decode — and is indeterminate.
fn send_failure(error: A2aError) -> A2aError {
    match &error {
        A2aError::HttpStatus { status, .. } if status.is_client_error() => error,
        A2aError::Connect | A2aError::Config(_) | A2aError::Rpc { .. } => error,
        _ => A2aError::SendIndeterminate,
    }
}
