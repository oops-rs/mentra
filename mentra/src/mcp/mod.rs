//! Model Context Protocol (MCP) client support.
//!
//! This module provides a generic MCP stdio client that can connect to external
//! MCP servers, discover their tools, and bridge those tools into the Mentra
//! runtime tool system.
//!
//! # Architecture
//!
//! - [`protocol`] — JSON-RPC 2.0 and MCP protocol types
//! - [`client`] — Stdio transport client for a single MCP server process
//! - [`bridge`] — Wraps MCP tools as Mentra [`ExecutableTool`] instances
//! - [`manager`] — Manages multiple MCP server connections and lifecycle

pub mod bridge;
pub mod client;
pub mod manager;
pub mod protocol;

#[cfg(test)]
mod tests;

pub use bridge::{McpBridgedTool, mcp_tool_name, parse_mcp_tool_name};
pub use client::{McpClientError, McpStdioClient};
pub use manager::{McpManager, McpServerStatus, McpServerSummary};
pub use protocol::{McpServerConfig, McpToolDefinition};
