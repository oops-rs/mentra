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
//! - [`protocol`] — JSON-RPC 2.0 and MCP protocol types shared by both transports
//! - [`client`] — stdio transport client for a single MCP server process
//! - [`sse`] — legacy HTTP+SSE transport client
//! - [`bridge`] — wraps MCP tools as Mentra [`ExecutableTool`] instances
//! - [`manager`] — manages multiple MCP server connections and lifecycle
//!
//! [`ExecutableTool`]: crate::tool::ExecutableTool

pub mod bridge;
pub mod client;
pub mod manager;
pub mod protocol;
pub mod sse;

#[cfg(test)]
mod registration_tests;
#[cfg(test)]
mod tests;

pub use bridge::{McpBridgedTool, mcp_tool_name, parse_mcp_tool_name};
pub use client::{McpClientError, McpStdioClient};
pub use manager::{McpManager, McpServerStatus, McpServerSummary};
pub use protocol::{McpServerConfig, McpToolDefinition};
pub use sse::client::{McpSseClient, McpSseError};
pub use sse::config::{McpSseConfigError, McpSseLimits, McpSseServerConfig, SecretString};
pub use sse::endpoint::EndpointError;
