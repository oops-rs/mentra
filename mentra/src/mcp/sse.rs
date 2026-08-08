//! Legacy MCP HTTP+SSE transport (protocol revision 2024-11-05).
//!
//! This is the transport MCP defined in revision 2024-11-05, where the client
//! holds a long-lived `GET` stream open and posts JSON-RPC messages to a
//! separate URL that the server names. It is distinct from Streamable HTTP;
//! see [`client`] for the differences and the full lifecycle.

pub(crate) mod client;
pub(crate) mod config;
pub(crate) mod endpoint;
#[cfg(test)]
pub(crate) mod testing;
pub(crate) mod wire;
