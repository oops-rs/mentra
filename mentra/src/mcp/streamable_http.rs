//! MCP Streamable HTTP transport (protocol revision 2025-03-26 and later).
//!
//! This is the transport that replaced HTTP+SSE. The server exposes a single
//! MCP endpoint; every JSON-RPC message is a `POST` to it, and the reply comes
//! back either as one `application/json` body or as a `text/event-stream` the
//! server opens in that same response. Session continuity, when the server
//! wants it, is an `Mcp-Session-Id` header rather than a URL parameter.
//!
//! See [`client`] for the lifecycle and [`sse`](crate::mcp::sse) for the older
//! transport this one supersedes.

pub mod client;
pub mod config;
#[cfg(test)]
pub(crate) mod testing;
