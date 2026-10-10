//! The two bindings this client speaks, and one operation in the terms of
//! both.
//!
//! The protocol defines each method once and every binding maps it onto its
//! own wire: JSON-RPC names it in a POST body, HTTP+JSON gives it a verb and
//! a path. An [`Operation`] carries both mappings, so a client method is
//! written once and the binding the card offered decides which is used.

use a2a_lf::{AgentCard, AgentInterface, TRANSPORT_PROTOCOL_HTTP_JSON, TRANSPORT_PROTOCOL_JSONRPC};
use reqwest::Method;
use serde::Serialize;
use serde_json::Value;

use crate::a2a::error::A2aError;

/// An A2A protocol binding this client speaks.
///
/// The specification requires an agent to serve only one of JSON-RPC,
/// HTTP+JSON and gRPC, so a client that speaks one reaches a fraction of
/// them. gRPC is left out for its weight: its stack would bring a second HTTP
/// client and a code generator into the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum A2aBinding {
    /// JSON-RPC 2.0: every method is a POST of an envelope to one URL.
    JsonRpc,
    /// HTTP+JSON: every method is its own verb and path under the
    /// interface's URL.
    HttpJson,
}

impl A2aBinding {
    /// The `protocolBinding` an Agent Card names this binding by.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::JsonRpc => TRANSPORT_PROTOCOL_JSONRPC,
            Self::HttpJson => TRANSPORT_PROTOCOL_HTTP_JSON,
        }
    }

    fn from_declared(declared: &str) -> Option<Self> {
        let declared = declared.trim();
        [Self::JsonRpc, Self::HttpJson]
            .into_iter()
            .find(|binding| declared.eq_ignore_ascii_case(binding.as_str()))
    }
}

impl std::fmt::Display for A2aBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The first interface this client can speak, with its binding.
///
/// The card lists interfaces in the agent's order of preference, so the first
/// match is the one the agent would choose; a later interface in a binding
/// this client prefers is not reached for over it.
pub(super) fn select_interface(card: &AgentCard) -> Option<(A2aBinding, &AgentInterface)> {
    card.supported_interfaces.iter().find_map(|interface| {
        let binding = A2aBinding::from_declared(&interface.protocol_binding)?;
        speaks_version(&interface.protocol_version).then_some((binding, interface))
    })
}

/// Whether a declared protocol version is one this client can speak to:
/// major version 1, at any minor or patch level.
///
/// Every request names the version it was written for in `A2A-Version`, and
/// the specification requires a server to answer in that version's terms or
/// refuse with `VersionNotSupportedError`. So an interface at 1.1 is still
/// spoken to as 1.0 rather than misread, and only the major version decides
/// whether to try.
pub(super) fn speaks_version(declared: &str) -> bool {
    let mut parts = declared.trim().split('.');
    let major = parts.next().and_then(|part| part.parse::<u32>().ok());
    let rest_numeric = parts.all(|part| part.parse::<u32>().is_ok());
    major == Some(1) && rest_numeric
}

/// One A2A operation, in the terms of both bindings.
#[derive(Debug)]
pub(super) struct Operation {
    /// The method's name: the JSON-RPC `method`, and how errors name it.
    pub(super) method: &'static str,
    /// The request message: the JSON-RPC `params`, and the HTTP+JSON body of
    /// a `POST`.
    pub(super) params: Value,
    /// The HTTP+JSON verb.
    pub(super) verb: Method,
    /// The HTTP+JSON path below the interface URL (and its tenant), as
    /// unencoded segments.
    pub(super) path: Vec<String>,
    /// The HTTP+JSON query, for a method whose request travels in it.
    pub(super) query: Vec<(&'static str, String)>,
}

impl Operation {
    pub(super) fn new<P: Serialize>(
        method: &'static str,
        params: &P,
        verb: Method,
        path: impl IntoIterator<Item = String>,
    ) -> Result<Self, A2aError> {
        Ok(Self {
            method,
            params: serde_json::to_value(params).map_err(|_| A2aError::Decode { method })?,
            verb,
            path: path.into_iter().collect(),
            query: Vec::new(),
        })
    }

    /// Adds a query parameter when the request sets it.
    pub(super) fn query(mut self, key: &'static str, value: Option<impl ToString>) -> Self {
        if let Some(value) = value {
            self.query.push((key, value.to_string()));
        }
        self
    }

    /// Whether the HTTP+JSON binding sends the request as a body.
    pub(super) fn has_body(&self) -> bool {
        self.verb == Method::POST
    }
}
