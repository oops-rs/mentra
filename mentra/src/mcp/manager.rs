//! Manages multiple MCP server connections and their lifecycle.

use std::collections::HashMap;
use std::sync::Arc;

use super::bridge::{McpBridgedTool, McpToolClient, mcp_tool_name};
use super::client::{McpClientError, McpStdioClient};
use super::protocol::{McpServerConfig, McpToolDefinition};
use super::sse::client::{McpSseClient, McpSseError};
use super::sse::config::McpSseServerConfig;
use super::streamable_http::client::{McpStreamableHttpClient, McpStreamableHttpError};
use super::streamable_http::config::McpStreamableHttpServerConfig;

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

/// A connected client, whichever transport it speaks.
///
/// The manager needs more than [`McpToolClient`] provides — it reports server
/// versions and shuts connections down — so the transports are held in an enum
/// rather than behind that trait.
enum TransportClient {
    Stdio(Arc<McpStdioClient>),
    Sse(Arc<McpSseClient>),
    StreamableHttp(Arc<McpStreamableHttpClient>),
}

impl TransportClient {
    /// The server version reported by the `initialize` handshake.
    fn server_version(&self) -> Option<String> {
        match self {
            Self::Stdio(client) => client.server_info().map(|info| info.version.clone()),
            Self::Sse(client) => client.server_info().map(|info| info.version.clone()),
            Self::StreamableHttp(client) => client.server_info().map(|info| info.version.clone()),
        }
    }

    /// Closes the connection.
    async fn shutdown(&self) {
        match self {
            Self::Stdio(client) => client.shutdown().await,
            Self::Sse(client) => client.shutdown().await,
            Self::StreamableHttp(client) => client.shutdown().await,
        }
    }

    /// Calls a tool, flattening the transport's error to a message.
    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Option<serde_json::Value>,
    ) -> Result<super::protocol::McpToolCallResult, String> {
        match self {
            Self::Stdio(client) => McpToolClient::call_tool(&**client, tool_name, arguments).await,
            Self::Sse(client) => McpToolClient::call_tool(&**client, tool_name, arguments).await,
            Self::StreamableHttp(client) => {
                McpToolClient::call_tool(&**client, tool_name, arguments).await
            }
        }
    }

    /// Bridges every advertised tool into a runtime tool.
    fn bridge(&self, server_name: &str, tools: &[McpToolDefinition]) -> Vec<McpBridgedTool> {
        tools
            .iter()
            .map(|tool| match self {
                Self::Stdio(client) => {
                    McpBridgedTool::new(server_name.to_string(), tool.clone(), client.clone())
                }
                Self::Sse(client) => {
                    McpBridgedTool::new(server_name.to_string(), tool.clone(), client.clone())
                }
                Self::StreamableHttp(client) => {
                    McpBridgedTool::new(server_name.to_string(), tool.clone(), client.clone())
                }
            })
            .collect()
    }
}

/// Tracks a connected MCP server.
struct ConnectedServer {
    client: TransportClient,
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

    /// Connect to an MCP server over stdio and discover its tools.
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

        let tools = client.tools().to_vec();
        let client = TransportClient::Stdio(Arc::new(client));

        Ok(self.register(config.name.clone(), client, tools))
    }

    /// Connect to an MCP server over the legacy HTTP+SSE transport and discover
    /// its tools.
    ///
    /// Returns the bridged tools ready for registration, exactly as
    /// [`connect`](Self::connect) does for stdio.
    pub async fn connect_sse(
        &mut self,
        config: &McpSseServerConfig,
    ) -> Result<Vec<McpBridgedTool>, McpSseError> {
        self.disconnect(&config.name).await;

        let client = McpSseClient::connect(config).await.inspect_err(|error| {
            self.errors.insert(config.name.clone(), error.to_string());
        })?;

        let tools = client.tools().to_vec();
        let client = TransportClient::Sse(Arc::new(client));

        Ok(self.register(config.name.clone(), client, tools))
    }

    /// Connect to an MCP server over the Streamable HTTP transport and discover
    /// its tools.
    ///
    /// This is the transport current MCP servers ship; a server that answers
    /// `404` on a legacy `/sse` path needs this rather than
    /// [`connect_sse`](Self::connect_sse). Returns the bridged tools ready for
    /// registration, exactly as [`connect`](Self::connect) does for stdio.
    pub async fn connect_streamable_http(
        &mut self,
        config: &McpStreamableHttpServerConfig,
    ) -> Result<Vec<McpBridgedTool>, McpStreamableHttpError> {
        self.disconnect(&config.name).await;

        let client = McpStreamableHttpClient::connect(config)
            .await
            .inspect_err(|error| {
                self.errors.insert(config.name.clone(), error.to_string());
            })?;

        let tools = client.tools().to_vec();
        let client = TransportClient::StreamableHttp(Arc::new(client));

        Ok(self.register(config.name.clone(), client, tools))
    }

    /// Records a connected server and bridges its tools.
    fn register(
        &mut self,
        name: String,
        client: TransportClient,
        tools: Vec<McpToolDefinition>,
    ) -> Vec<McpBridgedTool> {
        let bridged = client.bridge(&name, &tools);
        self.errors.remove(&name);
        self.servers.insert(name, ConnectedServer { client, tools });
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
                server_version: server.client.server_version(),
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
