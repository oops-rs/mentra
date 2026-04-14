//! MCP stdio client — spawns a child process and communicates via JSON-RPC over stdin/stdout.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::Value as JsonValue;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};

use super::protocol::*;

/// Default timeout for the MCP `initialize` handshake.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(10);
/// Default timeout for `tools/list`.
const LIST_TOOLS_TIMEOUT: Duration = Duration::from_secs(30);
/// Default timeout for `tools/call`.
const CALL_TOOL_TIMEOUT: Duration = Duration::from_secs(120);

/// Errors from the MCP stdio client.
#[derive(Debug, thiserror::Error)]
pub enum McpClientError {
    #[error("failed to spawn MCP server process: {0}")]
    SpawnFailed(#[from] std::io::Error),

    #[error("MCP server process has no stdin")]
    NoStdin,

    #[error("MCP server process has no stdout")]
    NoStdout,

    #[error("MCP server returned JSON-RPC error: {0}")]
    JsonRpc(JsonRpcError),

    #[error("timeout waiting for MCP response ({0:?})")]
    Timeout(Duration),

    #[error("MCP server process exited unexpectedly")]
    ProcessExited,

    #[error("failed to parse MCP response: {0}")]
    ParseError(String),

    #[error("MCP client is already shut down")]
    Shutdown,
}

type PendingMap = HashMap<u64, oneshot::Sender<Result<JsonValue, McpClientError>>>;

/// A running MCP stdio client connected to one server process.
pub struct McpStdioClient {
    stdin: Mutex<ChildStdin>,
    _child: Mutex<Child>,
    next_id: AtomicU64,
    pending: Arc<Mutex<PendingMap>>,
    server_info: Option<McpServerInfo>,
    tools: Vec<McpToolDefinition>,
    server_name: String,
}

impl McpStdioClient {
    /// Spawn the MCP server process and perform the `initialize` handshake.
    pub async fn connect(config: &McpServerConfig) -> Result<Self, McpClientError> {
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        for (key, value) in &config.env {
            cmd.env(key, value);
        }
        if let Some(cwd) = &config.cwd {
            cmd.current_dir(cwd);
        }

        let mut child = cmd.spawn()?;

        let stdin = child.stdin.take().ok_or(McpClientError::NoStdin)?;
        let stdout = child.stdout.take().ok_or(McpClientError::NoStdout)?;

        let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));

        // Spawn the reader task that routes responses to pending callers.
        let pending_clone = pending.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(trimmed) {
                    let id = match &resp.id {
                        JsonRpcId::Number(n) => *n,
                        _ => continue,
                    };
                    let mut pending = pending_clone.lock().await;
                    if let Some(tx) = pending.remove(&id) {
                        let result = if let Some(err) = resp.error {
                            Err(McpClientError::JsonRpc(err))
                        } else {
                            Ok(resp.result.unwrap_or(JsonValue::Null))
                        };
                        let _ = tx.send(result);
                    }
                }
            }
            // When the reader exits, signal all pending callers.
            let mut pending = pending_clone.lock().await;
            for (_, tx) in pending.drain() {
                let _ = tx.send(Err(McpClientError::ProcessExited));
            }
        });

        let mut client = Self {
            stdin: Mutex::new(stdin),
            _child: Mutex::new(child),
            next_id: AtomicU64::new(1),
            pending,
            server_info: None,
            tools: Vec::new(),
            server_name: config.name.clone(),
        };

        // Perform initialize handshake.
        client.initialize().await?;

        // Discover tools.
        client.discover_tools().await?;

        Ok(client)
    }

    /// Server name from the configuration.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// Server info returned by the `initialize` handshake.
    pub fn server_info(&self) -> Option<&McpServerInfo> {
        self.server_info.as_ref()
    }

    /// Tools discovered from this server.
    pub fn tools(&self) -> &[McpToolDefinition] {
        &self.tools
    }

    /// Send a JSON-RPC request and wait for the response.
    async fn call<P: serde::Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: Option<P>,
        timeout_duration: Duration,
    ) -> Result<R, McpClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let params_value = params
            .map(|p| serde_json::to_value(p).expect("serialize params"))
            .filter(|v| !v.is_null());

        let request = JsonRpcRequest::new(id, method, params_value);
        let mut line = serde_json::to_string(&request).expect("serialize request");
        line.push('\n');

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(id, tx);
        }

        {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(line.as_bytes())
                .await
                .map_err(|_| McpClientError::ProcessExited)?;
            stdin
                .flush()
                .await
                .map_err(|_| McpClientError::ProcessExited)?;
        }

        let result = tokio::time::timeout(timeout_duration, rx)
            .await
            .map_err(|_| McpClientError::Timeout(timeout_duration))?
            .map_err(|_| McpClientError::ProcessExited)??;

        serde_json::from_value(result)
            .map_err(|e| McpClientError::ParseError(format!("deserialize response: {e}")))
    }

    /// Send a JSON-RPC notification (no response expected).
    async fn notify<P: serde::Serialize>(
        &self,
        method: &str,
        params: Option<P>,
    ) -> Result<(), McpClientError> {
        // Notifications have no id — use a raw object.
        let mut obj = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
        });
        if let Some(p) = params {
            obj["params"] = serde_json::to_value(p).expect("serialize params");
        }
        let mut line = serde_json::to_string(&obj).expect("serialize notification");
        line.push('\n');

        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|_| McpClientError::ProcessExited)?;
        stdin
            .flush()
            .await
            .map_err(|_| McpClientError::ProcessExited)?;
        Ok(())
    }

    async fn initialize(&mut self) -> Result<(), McpClientError> {
        let params = McpInitializeParams {
            protocol_version: "2024-11-05".to_string(),
            capabilities: serde_json::json!({}),
            client_info: McpClientInfo {
                name: "mentra".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        };

        let result: McpInitializeResult = self
            .call("initialize", Some(params), INITIALIZE_TIMEOUT)
            .await?;

        self.server_info = Some(result.server_info);

        // Send initialized notification.
        self.notify::<JsonValue>("notifications/initialized", None)
            .await?;

        Ok(())
    }

    async fn discover_tools(&mut self) -> Result<(), McpClientError> {
        let mut all_tools = Vec::new();
        let mut cursor: Option<String> = None;

        loop {
            let params = McpListToolsParams {
                cursor: cursor.clone(),
            };
            let result: McpListToolsResult = self
                .call("tools/list", Some(params), LIST_TOOLS_TIMEOUT)
                .await?;

            all_tools.extend(result.tools);

            match result.next_cursor {
                Some(next) if !next.is_empty() => cursor = Some(next),
                _ => break,
            }
        }

        self.tools = all_tools;
        Ok(())
    }

    /// Call a tool on this server.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Option<JsonValue>,
    ) -> Result<McpToolCallResult, McpClientError> {
        let params = McpToolCallParams {
            name: tool_name.to_string(),
            arguments,
        };
        self.call("tools/call", Some(params), CALL_TOOL_TIMEOUT)
            .await
    }

    /// Shut down the MCP server process gracefully.
    pub async fn shutdown(&self) {
        // Best-effort: drop stdin to signal the child.
        let mut stdin = self.stdin.lock().await;
        drop(stdin.shutdown().await);
    }
}

impl Drop for McpStdioClient {
    fn drop(&mut self) {
        // The child process will be killed when the Child handle is dropped.
    }
}
