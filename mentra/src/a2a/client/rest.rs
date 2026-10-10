//! The HTTP+JSON binding: every method is its own verb and path under the
//! interface's URL.
//!
//! Paths are the `google.api.http` bindings of the normative `a2a.proto`,
//! with the tenant as the first path segment when the interface names one
//! (`/{tenant}/tasks/{id}`). A request message travels as the body of a
//! `POST`, and in the query of a `GET`, with camelCase keys. A failure is a
//! `google.rpc.Status` whose `ErrorInfo` detail names the A2A error by reason.

#[cfg(test)]
mod tests;

use a2a_lf::{ERROR_INFO_TYPE, PROTOCOL_DOMAIN, reason_to_error_code};
use reqwest::StatusCode;
use reqwest::header::ACCEPT;
use serde::Deserialize;
use serde_json::Value;

use super::Transport;
use super::binding::Operation;
use crate::a2a::config::A2aUrlError;
use crate::a2a::error::{A2aError, A2aRpcError};

impl Transport {
    /// The request carrying `operation` over HTTP+JSON.
    pub(super) fn rest_request(
        &self,
        operation: &Operation,
        accept: &'static str,
    ) -> Result<reqwest::RequestBuilder, A2aError> {
        let url = self.rest_url(operation)?;
        let request = self
            .http
            .request(operation.verb.clone(), url)
            .headers(self.headers.clone())
            .header(ACCEPT, accept);
        Ok(if operation.has_body() {
            request.json(&operation.params)
        } else {
            request
        })
    }

    /// The interface URL, then the tenant, then the operation's path, each
    /// segment percent-encoded so an id the agent chose cannot add segments
    /// of its own.
    fn rest_url(&self, operation: &Operation) -> Result<url::Url, A2aError> {
        let mut url = self.endpoint.clone();
        {
            // The endpoint was checked to be http(s) with a host, which
            // always has path segments.
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| A2aError::InvalidEndpoint(A2aUrlError::Malformed))?;
            segments.pop_if_empty();
            segments.extend(self.tenant.iter());
            segments.extend(operation.path.iter());
        }
        if !operation.query.is_empty() {
            url.query_pairs_mut().extend_pairs(operation.query.iter());
        }
        Ok(url)
    }
}

/// Decodes a reply: the body of a success (`Null` when empty), or the error a
/// failure names.
pub(super) fn decode_reply(
    method: &'static str,
    status: StatusCode,
    body: &[u8],
) -> Result<Value, A2aError> {
    if !status.is_success() {
        return Err(match a2a_error(body) {
            Some(error) => A2aError::Rpc { method, error },
            None => A2aError::HttpStatus {
                operation: method,
                status,
            },
        });
    }
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Null);
    }
    serde_json::from_slice(body).map_err(|_| A2aError::Decode { method })
}

/// The A2A error a `google.rpc.Status` body names by its `ErrorInfo` reason.
///
/// Only the reason is read — never the message — and only from a detail in
/// the A2A domain, so a gateway's own error of the same shape is reported by
/// its HTTP status instead.
pub(super) fn a2a_error(body: &[u8]) -> Option<A2aRpcError> {
    let envelope: StatusEnvelope = serde_json::from_slice(body).ok()?;
    envelope.error.a2a_error()
}

/// `{"error": google.rpc.Status}`.
#[derive(Deserialize)]
pub(super) struct StatusEnvelope {
    pub(super) error: Status,
}

/// The part of a `google.rpc.Status` this client reads.
#[derive(Deserialize)]
pub(super) struct Status {
    #[serde(default)]
    details: Vec<Value>,
}

impl Status {
    pub(super) fn a2a_error(&self) -> Option<A2aRpcError> {
        self.details.iter().find_map(|detail| {
            let field = |name: &str| detail.get(name).and_then(Value::as_str);
            if field("@type") != Some(ERROR_INFO_TYPE) || field("domain") != Some(PROTOCOL_DOMAIN) {
                return None;
            }
            reason_to_error_code(field("reason")?).map(A2aRpcError::from_code)
        })
    }
}
