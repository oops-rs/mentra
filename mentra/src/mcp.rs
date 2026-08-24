//! Model Context Protocol (MCP) client support.
//!
//! This module provides generic MCP clients that connect to external MCP
//! servers, discover their tools, and bridge those tools into the Mentra
//! runtime tool system.
//!
//! # Transports
//!
//! Two transports are supported, chosen by which configuration type you use:
//!
//! - **stdio** — [`McpServerConfig`] spawns a child process and speaks JSON-RPC
//!   over its standard input and output.
//! - **legacy HTTP+SSE** — [`McpSseServerConfig`] opens a long-lived
//!   `text/event-stream` `GET` and posts JSON-RPC messages to a second URL that
//!   the server names. This is the transport from protocol revision
//!   2024-11-05, not Streamable HTTP; see [`McpSseClient`] for the distinction.
//!
//! # Architecture
//!
//! These links use absolute paths because the module's documentation is merged
//! with the outer comment on its `pub mod` declaration, which resolves relative
//! links against the crate root rather than this module.
//!
//! - [`protocol`](crate::mcp::protocol) — JSON-RPC 2.0 and MCP protocol types
//!   shared by both transports
//! - [`client`](crate::mcp::client) — stdio transport client for a single MCP
//!   server process
//! - [`sse`](crate::mcp::sse) — legacy HTTP+SSE transport client
//! - [`bridge`](crate::mcp::bridge) — wraps MCP tools as Mentra
//!   [`ExecutableTool`] instances
//! - [`manager`](crate::mcp::manager) — manages multiple MCP server connections
//!   and lifecycle
//!
//! [`ExecutableTool`]: crate::tool::ExecutableTool

pub mod bridge;
pub mod client;
pub mod manager;
pub mod protocol;
pub mod secret;
pub mod sse;
pub mod streamable_http;

#[cfg(test)]
mod registration_tests;
#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;

pub use bridge::{McpBridgedTool, mcp_tool_name, parse_mcp_tool_name};
pub use client::{McpClientError, McpStdioClient};
pub use manager::{McpManager, McpServerStatus, McpServerSummary};
pub use protocol::{McpServerConfig, McpToolDefinition};
pub use secret::SecretString;
pub use sse::client::{McpSseClient, McpSseError};
pub use sse::config::{McpSseConfigError, McpSseLimits, McpSseServerConfig};
pub use sse::endpoint::EndpointError;
pub use streamable_http::config::{
    McpStreamableHttpConfigError, McpStreamableHttpLimits, McpStreamableHttpServerConfig,
};
