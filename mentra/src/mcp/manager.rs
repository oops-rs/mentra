//! Manages multiple MCP server connections and their lifecycle.

use std::collections::HashMap;
use std::sync::Arc;

use super::bridge::{McpBridgedTool, McpToolClient, mcp_tool_name, validate_mcp_server_name};
use super::client::{McpClientError, McpStdioClient};
use super::protocol::{McpServerConfig, McpToolDefinition};
use super::sse::client::{McpSseClient, McpSseError};
use super::sse::config::McpSseServerConfig;
use super::streamable_http::client::{McpStreamableHttpClient, McpStreamableHttpError};
use super::streamable_http::config::McpStreamableHttpServerConfig;

/// Status of an MCP server connection.
///
/// These are the two states [`McpManager::list_servers`] distinguishes: a live
/// entry in `servers` reports `Connected`, and a name present only in `errors`
/// reports `Error`. A server that is not connected is absent from the listing
/// entirely, and connecting is synchronous within the `connect*` methods, so no
/// intermediate state is ever observable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpServerStatus {
    Connected,
    Error,
}

impl std::fmt::Display for McpServerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connected => write!(f, "connected"),
            Self::Error => write!(f, "error"),
        }
    }
}

/// Summary of a managed MCP server.
#[derive(Debug, Clone)]
pub struct McpServerSummary {
    pub name: String,
    pub status: McpServerStatus,
    pub server_version: Option<String>,
    pub tool_count: usize,
    pub error: Option<String>,
}

/// Tracks a connected MCP server.
struct ConnectedServer {
    client: Arc<dyn McpToolClient>,
    tools: Vec<McpToolDefinition>,
    /// Version reported by the `initialize` handshake, snapshotted at connect
    /// time; the client never revises it afterwards.
    server_version: Option<String>,
}

/// Manages the lifecycle of multiple MCP server processes.
pub struct McpManager {
    servers: HashMap<String, ConnectedServer>,
    errors: HashMap<String, String>,
}

impl McpManager {
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
            errors: HashMap::new(),
        }
    }

    /// Connect to an MCP server over stdio and discover its tools.
    /// Returns the bridged tools ready for registration.
    ///
    /// Rejects `config.name` before connecting if
    /// [`validate_mcp_server_name`] would reject it, so a name shaped to
    /// collide with another server's tools under [`mcp_tool_name`] never
    /// reaches a live connection.
    pub async fn connect(
        &mut self,
        config: &McpServerConfig,
    ) -> Result<Vec<McpBridgedTool>, McpClientError> {
        validate_mcp_server_name(&config.name)?;

        // Disconnect existing connection if any.
        self.disconnect(&config.name).await;

        let client = McpStdioClient::connect(config).await.inspect_err(|e| {
            self.errors.insert(config.name.clone(), e.to_string());
        })?;

        let tools = client.tools().to_vec();
        let server_version = client.server_info().map(|info| info.version.clone());
        let client: Arc<dyn McpToolClient> = Arc::new(client);

        Ok(self.register(config.name.clone(), client, tools, server_version))
    }

    /// Connect to an MCP server over the legacy HTTP+SSE transport and discover
    /// its tools.
    ///
    /// Returns the bridged tools ready for registration, exactly as
    /// [`connect`](Self::connect) does for stdio, including the server-name
    /// validation.
    pub async fn connect_sse(
        &mut self,
        config: &McpSseServerConfig,
    ) -> Result<Vec<McpBridgedTool>, McpSseError> {
        validate_mcp_server_name(&config.name)?;

        self.disconnect(&config.name).await;

        let client = McpSseClient::connect(config).await.inspect_err(|error| {
            self.errors.insert(config.name.clone(), error.to_string());
        })?;

        let tools = client.tools().to_vec();
        let server_version = client.server_info().map(|info| info.version.clone());
        let client: Arc<dyn McpToolClient> = Arc::new(client);

        Ok(self.register(config.name.clone(), client, tools, server_version))
    }

    /// Connect to an MCP server over the Streamable HTTP transport and discover
    /// its tools.
    ///
    /// This is the transport current MCP servers ship; a server that answers
    /// `404` on a legacy `/sse` path needs this rather than
    /// [`connect_sse`](Self::connect_sse). Returns the bridged tools ready for
    /// registration, exactly as [`connect`](Self::connect) does for stdio,
    /// including the server-name validation.
    pub async fn connect_streamable_http(
        &mut self,
        config: &McpStreamableHttpServerConfig,
    ) -> Result<Vec<McpBridgedTool>, McpStreamableHttpError> {
        validate_mcp_server_name(&config.name)?;

        self.disconnect(&config.name).await;

        let client = McpStreamableHttpClient::connect(config)
            .await
            .inspect_err(|error| {
                self.errors.insert(config.name.clone(), error.to_string());
            })?;

        let tools = client.tools().to_vec();
        let server_version = client.server_info().map(|info| info.version.clone());
        let client: Arc<dyn McpToolClient> = Arc::new(client);

        Ok(self.register(config.name.clone(), client, tools, server_version))
    }

    /// Records a connected server and bridges its tools.
    fn register(
        &mut self,
        name: String,
        client: Arc<dyn McpToolClient>,
        tools: Vec<McpToolDefinition>,
        server_version: Option<String>,
    ) -> Vec<McpBridgedTool> {
        let bridged: Vec<McpBridgedTool> = tools
            .iter()
            .map(|tool| McpBridgedTool::new(name.clone(), tool.clone(), client.clone()))
            .collect();
        self.errors.remove(&name);
        self.servers.insert(
            name,
            ConnectedServer {
                client,
                tools,
                server_version,
            },
        );
        bridged
    }

    /// Disconnect a server by name.
    pub async fn disconnect(&mut self, name: &str) {
        if let Some(server) = self.servers.remove(name) {
            server.client.shutdown().await;
        }
    }

    /// Shut down all connected servers.
    pub async fn shutdown_all(&mut self) {
        let names: Vec<String> = self.servers.keys().cloned().collect();
        for name in names {
            self.disconnect(&name).await;
        }
    }

    /// List all server summaries.
    pub fn list_servers(&self) -> Vec<McpServerSummary> {
        let mut summaries: Vec<McpServerSummary> = self
            .servers
            .iter()
            .map(|(name, server)| McpServerSummary {
                name: name.clone(),
                status: McpServerStatus::Connected,
                server_version: server.server_version.clone(),
                tool_count: server.tools.len(),
                error: None,
            })
            .collect();

        // Include errored servers.
        for (name, error) in &self.errors {
            if !self.servers.contains_key(name) {
                summaries.push(McpServerSummary {
                    name: name.clone(),
                    status: McpServerStatus::Error,
                    server_version: None,
                    tool_count: 0,
                    error: Some(error.clone()),
                });
            }
        }

        summaries.sort_by(|a, b| a.name.cmp(&b.name));
        summaries
    }

    /// Get the namespaced tool names for all connected servers.
    pub fn all_tool_names(&self) -> Vec<String> {
        self.servers
            .iter()
            .flat_map(|(name, server)| {
                server
                    .tools
                    .iter()
                    .map(move |tool| mcp_tool_name(name, &tool.name))
            })
            .collect()
    }

    /// Call a tool on a specific server, whichever transport it speaks.
    ///
    /// Each transport reports failures with its own error type, so this returns
    /// the message rather than widening the error into a shared enum.
    pub async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments: Option<serde_json::Value>,
    ) -> Result<super::protocol::McpToolCallResult, String> {
        let server = self
            .servers
            .get(server_name)
            .ok_or_else(|| format!("MCP server '{server_name}' not connected"))?;

        server.client.call_tool(tool_name, arguments).await
    }

    /// Check if a server is connected.
    pub fn is_connected(&self, name: &str) -> bool {
        self.servers.contains_key(name)
    }

    /// Number of connected servers.
    pub fn connected_count(&self) -> usize {
        self.servers.len()
    }
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}
