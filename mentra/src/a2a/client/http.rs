//! The HTTP underneath both bindings: one client, its headers, and bounded
//! reads.

use futures_util::StreamExt;
use reqwest::StatusCode;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use super::A2A_PROTOCOL_VERSION;
use crate::a2a::config::{A2aAgentConfig, A2aConfigError, A2aLimits};
use crate::a2a::error::A2aError;

/// The header naming the protocol version.
const VERSION_HEADER: &str = "a2a-version";

/// Sends one request and reads the whole reply, bounded in time and size.
///
/// One deadline covers the request and the body, so an agent cannot evade
/// `request_timeout` by sending the head and withholding the body.
pub(super) async fn exchange(
    request: reqwest::RequestBuilder,
    limits: &A2aLimits,
) -> Result<(StatusCode, Vec<u8>), A2aError> {
    let operation = async {
        let response = send(request).await?;
        let status = response.status();
        let body = read_bounded(response, limits.max_response_bytes).await?;
        Ok((status, body))
    };
    tokio::time::timeout(limits.request_timeout, operation)
        .await
        .map_err(|_| A2aError::Timeout(limits.request_timeout))?
}

/// Sends one request and returns once the reply's head has arrived, refusing
/// a redirect.
pub(super) async fn send(request: reqwest::RequestBuilder) -> Result<reqwest::Response, A2aError> {
    let response = request.send().await.map_err(transport_error)?;
    if response.status().is_redirection() {
        return Err(A2aError::RedirectRefused);
    }
    Ok(response)
}

/// Reads a response body, refusing one larger than `limit`.
pub(super) async fn read_bounded(
    response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, A2aError> {
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
pub(super) fn build_http_client(limits: &A2aLimits) -> Result<reqwest::Client, A2aError> {
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
pub(super) fn build_headers(config: &A2aAgentConfig) -> Result<HeaderMap, A2aError> {
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
pub(super) fn transport_error(error: reqwest::Error) -> A2aError {
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
