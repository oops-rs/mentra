//! Manages multiple MCP server connections and their lifecycle.

use std::collections::HashMap;
use std::sync::Arc;

use super::bridge::{McpBridgedTool, mcp_tool_name};
use super::client::{McpClientError, McpStdioClient};
use super::protocol::{McpServerConfig, McpToolDefinition};

/// Status of an MCP server connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpServerStatus {
    Disconnected,
    Connecting,
    Connected,
    Error,
}

impl std::fmt::Display for McpServerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disconnected => write!(f, "disconnected"),
            Self::Connecting => write!(f, "connecting"),
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
    client: Arc<McpStdioClient>,
    tools: Vec<McpToolDefinition>,
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

    /// Connect to an MCP server and discover its tools.
    /// Returns the bridged tools ready for registration.
    pub async fn connect(
        &mut self,
        config: &McpServerConfig,
    ) -> Result<Vec<McpBridgedTool>, McpClientError> {
        // Disconnect existing connection if any.
        self.disconnect(&config.name).await;

        let client = McpStdioClient::connect(config).await.inspect_err(|e| {
            self.errors.insert(config.name.clone(), e.to_string());
        })?;

        let client = Arc::new(client);
        let tools = client.tools().to_vec();

        let bridged: Vec<McpBridgedTool> = tools
            .iter()
            .map(|tool_def| {
                McpBridgedTool::new(config.name.clone(), tool_def.clone(), client.clone())
            })
            .collect();

        self.servers.insert(
            config.name.clone(),
            ConnectedServer {
                client,
                tools: tools.clone(),
            },
        );
        self.errors.remove(&config.name);

        Ok(bridged)
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
                server_version: server.client.server_info().map(|info| info.version.clone()),
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

    /// Call a tool on a specific server.
    pub async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments: Option<serde_json::Value>,
    ) -> Result<super::protocol::McpToolCallResult, McpClientError> {
        let server = self.servers.get(server_name).ok_or_else(|| {
            McpClientError::ParseError(format!("MCP server '{}' not connected", server_name))
        })?;

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
