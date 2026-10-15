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
//! - [`A2aClient`](crate::a2a::A2aClient) fetches and validates an agent's card and speaks
//!   every A2A method to it, over the JSON-RPC or the HTTP+JSON binding —
//!   whichever the card offers first.
//! - [`exchange`](crate::a2a::exchange()) submits a message, or resumes a task, and
//!   waits on the task — following its stream when the agent offers one,
//!   polling otherwise — until it settles or the client's wait bound passes,
//!   returning what happened as data.
//! - [`A2aAgentTool`](crate::a2a::A2aAgentTool) bridges one connected agent into the tool roster as
//!   `a2a__{agent}`: that exchange, with the text a model reads.
//!
//! # Scope
//!
//! Protocol 1.0, over two of its three bindings.
//!
//! - **Bindings.** [`A2aClient::connect`](crate::a2a::A2aClient::connect) selects the first interface,
//!   in the card's own order, whose binding is `JSONRPC` or `HTTP+JSON` and
//!   whose protocol major version is 1, and sends that interface's `tenant`
//!   on every request. gRPC is not spoken: its stack would bring a second
//!   HTTP client and a code generator into the tree.
//! - **Version.** Every request carries `A2A-Version: 1.0`, the card fetch
//!   included, so an agent at a later 1.x answers in 1.0's terms or refuses.
//! - **Methods.** All eleven: `SendMessage`, `SendStreamingMessage`,
//!   `GetTask`, `ListTasks`, `CancelTask`, `SubscribeToTask`, the four
//!   push-notification-config methods, and `GetExtendedAgentCard` — which
//!   `connect` calls itself when the card declares an extended card and
//!   credentials are configured.
//! - **Streaming.** Both bindings stream over server-sent events, each event
//!   bounded in size and the stream in idle time
//!   ([`A2aLimits`](crate::a2a::A2aLimits)).
//! - **Extensions.** A message carries `metadata`, and the extensions it
//!   lists are activated for its request in `A2A-Extensions`; this module
//!   knows no extension itself.
//! - **Push notifications** are configured, not received: receiving the
//!   agent's webhook calls is the host's own server's job.
//!
//! # Architecture
//!
//! These links use absolute paths because the module's documentation is merged
//! with the outer comment on its `pub mod` declaration, which resolves relative
//! links against the crate root rather than this module.
//!
//! - [`config`](crate::a2a::config) — [`A2aAgentConfig`](crate::a2a::A2aAgentConfig), how its URL names
//!   the card, and the limits a host tunes
//! - [`client`](crate::a2a::client) — the client for one agent, over either binding, and
//!   its event streams
//! - [`error`](crate::a2a::error) — [`A2aError`](crate::a2a::A2aError) and the A2A errors, by their
//!   JSON-RPC codes
//! - [`exchange`](mod@crate::a2a::exchange) — one request waited on until it settles, as data
//! - [`output`](crate::a2a::output) — a remote agent's words framed apart from the harness's
//! - [`tool`](crate::a2a::tool) — [`A2aAgentTool`](crate::a2a::A2aAgentTool), the call contract and
//!   what a reply looks like to the model
//! - [`name`](crate::a2a::name) — the `a2a__{agent}` tool name and the rule
//!   an agent's name must pass
//! - [`types`](crate::a2a::types) — the A2A wire types, re-exported from the
//!   official `a2a-lf` crate so a caller needs no direct dependency on it

pub mod client;
pub mod config;
pub mod error;
pub mod exchange;
pub mod name;
pub mod output;
pub mod tool;

#[cfg(test)]
pub(crate) mod testing;

/// The A2A v1.0 wire types: the official `a2a-lf` crate, re-exported.
///
/// [`A2aClient`] takes and returns these directly, so a caller building a
/// [`Message`](types::Message) or reading a [`Task`](types::Task) uses the
/// same version of them that this crate was built against.
pub use a2a_lf as types;

pub use client::{
    A2A_PROTOCOL_VERSION, A2aBinding, A2aClient, A2aEventStream, A2aExtendedCardStatus,
};
pub use config::{A2aAgentConfig, A2aConfigError, A2aLimits, A2aUrlError};
pub use error::{A2aError, A2aRpcError};
pub use exchange::{
    A2aExchange, A2aExchangeError, A2aRequest, A2aSend, A2aStillWorking, A2aSubmitted, exchange,
    is_settled, settle, state_name, submit,
};
pub use name::{A2aAgentNameError, a2a_tool_name, parse_a2a_tool_name, validate_a2a_agent_name};
pub use output::{card_summary, remote_files, remote_output, remote_reply, reply_files};
pub use tool::A2aAgentTool;
