//! The JSON-RPC binding: every method is one envelope posted to the
//! interface's URL.

use a2a_lf::{JsonRpcId, JsonRpcRequest, JsonRpcResponse};
use reqwest::StatusCode;
use reqwest::header::ACCEPT;
use serde_json::Value;

use super::Transport;
use super::binding::Operation;
use crate::a2a::error::{A2aError, A2aRpcError};

impl Transport {
    /// The POST carrying `operation` as a JSON-RPC request with id `id`.
    pub(super) fn jsonrpc_request(
        &self,
        operation: &Operation,
        id: i64,
        accept: &'static str,
    ) -> reqwest::RequestBuilder {
        let request = JsonRpcRequest::new(
            JsonRpcId::Number(id),
            operation.method,
            Some(operation.params.clone()),
        );
        self.http
            .post(self.endpoint.clone())
            .headers(self.headers.clone())
            .header(ACCEPT, accept)
            .json(&request)
    }
}

/// Decodes the reply to the request with id `id`, returning its `result`
/// (`Null` when it has none).
pub(super) fn decode_reply(
    method: &'static str,
    id: i64,
    status: StatusCode,
    body: &[u8],
) -> Result<Value, A2aError> {
    let Ok(reply) = serde_json::from_slice::<JsonRpcResponse>(body) else {
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
    let result = decode_envelope(method, id, reply)?;
    if !status.is_success() {
        return Err(A2aError::HttpStatus {
            operation: method,
            status,
        });
    }
    Ok(result)
}

/// Checks one JSON-RPC response is about request `id` and returns its
/// `result`, or the error it carries.
pub(super) fn decode_envelope(
    method: &'static str,
    id: i64,
    reply: JsonRpcResponse,
) -> Result<Value, A2aError> {
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
    Ok(reply.result.unwrap_or(Value::Null))
}
