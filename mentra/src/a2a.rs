//! Agent2Agent (A2A) protocol client support.
//!
//! [A2A v1.0](https://a2a-protocol.org/v1.0.0/specification) is how one agent
//! hands work to another whose insides it cannot see: the remote agent
//! publishes an Agent Card describing itself, and accepts messages that start
//! or continue *tasks* — server-owned units of work with a lifecycle that ends
//! in a terminal state, or pauses when the agent needs input. It is the
//! neighbour of MCP rather than an overlap: MCP is an agent calling tools, A2A
//! is an agent calling another agent.
//!
//! This module is the client half, behind the default-off `a2a` feature:
//!
//! - [`A2aClient`] fetches and validates an agent's card and speaks the
//!   JSON-RPC binding — `SendMessage`, `GetTask`, `CancelTask` — over HTTP.
//! - [`A2aAgentTool`] bridges one connected agent into the tool roster as
//!   `a2a__{agent}`, submitting a message and polling the task it starts until
//!   it settles or the call's wait bound passes.
//!
//! # Scope
//!
//! One binding and one version: JSON-RPC at protocol 1.0, the only interface
//! [`A2aClient::connect`] will select from a card. Every request carries
//! `A2A-Version: 1.0`. The streaming methods, push notifications, the extended
//! card, and the REST and gRPC bindings are not implemented.
//!
//! # Architecture
//!
//! These links use absolute paths because the module's documentation is merged
//! with the outer comment on its `pub mod` declaration, which resolves relative
//! links against the crate root rather than this module.
//!
//! - [`config`](crate::a2a::config) — [`A2aAgentConfig`], how its URL names
//!   the card, and the limits a host tunes
//! - [`client`](crate::a2a::client) — the JSON-RPC client for one agent
//! - [`error`](crate::a2a::error) — [`A2aError`] and the A2A JSON-RPC codes
//! - [`tool`](crate::a2a::tool) — [`A2aAgentTool`], the call contract and
//!   what a reply looks like to the model
//! - [`name`](crate::a2a::name) — the `a2a__{agent}` tool name and the rule
//!   an agent's name must pass
//! - [`types`](crate::a2a::types) — the A2A wire types, re-exported from the
//!   official `a2a-lf` crate so a caller needs no direct dependency on it

pub mod client;
pub mod config;
pub mod error;
pub mod name;
pub mod tool;

#[cfg(test)]
pub(crate) mod testing;

/// The A2A v1.0 wire types: the official `a2a-lf` crate, re-exported.
///
/// [`A2aClient`] takes and returns these directly, so a caller building a
/// [`Message`](types::Message) or reading a [`Task`](types::Task) uses the
/// same version of them that this crate was built against.
pub use a2a_lf as types;

pub use client::{A2A_PROTOCOL_VERSION, A2aClient};
pub use config::{A2aAgentConfig, A2aConfigError, A2aLimits, A2aUrlError};
pub use error::{A2aError, A2aRpcError};
pub use name::{A2aAgentNameError, a2a_tool_name, parse_a2a_tool_name, validate_a2a_agent_name};
pub use tool::A2aAgentTool;
