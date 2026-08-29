//! MCP stdio client — spawns a child process and communicates via JSON-RPC over stdin/stdout.

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::Value as JsonValue;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;
use tokio::sync::{Mutex, oneshot};

use crate::process::{BoundedChild, BoundedCommand, baseline_environment};

use super::protocol::*;

/// Default timeout for the MCP `initialize` handshake.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(10);
/// Default timeout for `tools/list`.
const LIST_TOOLS_TIMEOUT: Duration = Duration::from_secs(30);
/// Default timeout for `tools/call`.
const CALL_TOOL_TIMEOUT: Duration = Duration::from_secs(120);
/// Bound on how many `tools/list` pages are followed.
///
/// Cursors are opaque, so a server repeating one cannot be detected by value;
/// only a page bound stops the walk.
const MAX_TOOL_PAGES: usize = 1_000;

/// Cap on how many tools a server may advertise in total.
///
/// The page cap bounds the round trips; nothing bounded the list they
/// accumulate into. Well past what any real server exposes.
const MAX_TOOLS: usize = 4_096;

/// Maximum bytes accepted for one newline-delimited JSON-RPC frame from a
/// stdio server. The reader enforces this before parsing, so malformed or
/// hostile output cannot make one allocation grow without bound.
const MAX_SERVER_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

/// Stderr is diagnostic output rather than protocol data; drain it forever but
/// retain only a bounded head/tail for the lifetime of the client.
const MAX_SERVER_STDERR_BYTES: usize = 2 * 1024;

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

    #[error("MCP server kept paginating tools/list past {limit} pages")]
    TooManyToolPages { limit: usize },

    #[error("MCP server advertised more than {limit} tools")]
    TooManyTools { limit: usize },

    #[error("MCP client is already shut down")]
    Shutdown,

    #[error(transparent)]
    InvalidServerName(#[from] super::bridge::McpServerNameError),
}

type PendingMap = HashMap<u64, oneshot::Sender<Result<JsonValue, McpClientError>>>;

/// A running MCP stdio client connected to one server process.
pub struct McpStdioClient {
    stdin: Mutex<ChildStdin>,
    child: Arc<Mutex<BoundedChild>>,
    next_id: AtomicU64,
    pending: Arc<Mutex<PendingMap>>,
    server_info: Option<McpServerInfo>,
    tools: Vec<McpToolDefinition>,
    server_name: String,
}

impl McpStdioClient {
    /// Spawn the MCP server process and perform the `initialize` handshake.
    ///
    /// The process starts with an empty environment, then receives the
    /// cross-platform runnable baseline and the variables in `config.env`.
    /// On Unix the baseline is `PATH`, `HOME`, `TMPDIR`, `TMP`, `TEMP`, `LANG`,
    /// and `LC_ALL`; on Windows it is `PATH`, `PATHEXT`, `SystemRoot`,
    /// `COMSPEC`, `TEMP`, and `TMP`. A `.mcp.json` author must name every
    /// other variable explicitly. The server runs in its own host process
    /// session/tree and is terminated as a unit when this client is dropped or
    /// shut down; this is process hygiene, not a sandbox or confinement
    /// boundary.
    pub async fn connect(config: &McpServerConfig) -> Result<Self, McpClientError> {
        let overrides: Vec<_> = config
            .env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let baseline = baseline_environment()
            .into_iter()
            .filter(|(key, _)| !config.env.contains_key(&key.to_string_lossy().into_owned()));
        let command = BoundedCommand::new(
            &config.command,
            Duration::from_secs(0),
            MAX_SERVER_MESSAGE_BYTES,
        )
        .args(&config.args)
        .envs(baseline)
        .envs(overrides)
        .max_stderr_bytes(MAX_SERVER_STDERR_BYTES);
        if let Some(cwd) = &config.cwd {
            let command = command.current_dir(cwd);
            return Self::connect_with_command(command, config).await;
        }

        Self::connect_with_command(command, config).await
    }

    async fn connect_with_command(
        command: BoundedCommand,
        config: &McpServerConfig,
    ) -> Result<Self, McpClientError> {
        let mut child = command.spawn_piped()?;

        let stdin = child.take_stdin().ok_or(McpClientError::NoStdin)?;
        let stdout = child.take_stdout().ok_or(McpClientError::NoStdout)?;
        let child = Arc::new(Mutex::new(child));

        let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));

        // Spawn the reader task that routes responses to pending callers.
        let pending_clone = pending.clone();
        let child_weak = Arc::downgrade(&child);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                let line = match read_bounded_line(&mut reader, MAX_SERVER_MESSAGE_BYTES).await {
                    Ok(Some(line)) => line,
                    Ok(None) | Err(_) => break,
                };
                let Ok(line) = String::from_utf8(line) else {
                    break;
                };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(trimmed) {
                    // A response carries a result or an error. A server-initiated
                    // request such as `ping` also has an id, and without this
                    // check it would resolve the caller holding that id with a
                    // null result.
                    if resp.result.is_none() && resp.error.is_none() {
                        continue;
                    }
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

            // EOF, an I/O error, an oversized frame, or invalid UTF-8 makes
            // this protocol stream unusable. A strong child owner remains in
            // the client, while this task holds only a Weak reference so it
            // cannot keep the process alive after the client is dropped.
            if let Some(child) = child_weak.upgrade() {
                let mut child = child.lock().await;
                drop(child.terminate().await);
            }

            // When the reader exits, signal all pending callers.
            let mut pending = pending_clone.lock().await;
            for (_, tx) in pending.drain() {
                let _ = tx.send(Err(McpClientError::ProcessExited));
            }
        });

        let mut client = Self {
            stdin: Mutex::new(stdin),
            child,
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
            if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                // The request never reached the server, so drop its
                // registration rather than leaving it to time out.
                self.pending.lock().await.remove(&id);
                return Err(McpClientError::ProcessExited);
            }
        }

        let result = match tokio::time::timeout(timeout_duration, rx).await {
            Ok(Ok(result)) => result?,
            Ok(Err(_)) => return Err(McpClientError::ProcessExited),
            Err(_) => {
                // Remove the registration so a timed-out request cannot leak an
                // entry for the lifetime of the connection.
                self.pending.lock().await.remove(&id);
                return Err(McpClientError::Timeout(timeout_duration));
            }
        };

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
        let mut pages = 0_usize;

        loop {
            let params = McpListToolsParams {
                cursor: cursor.clone(),
            };
            let result: McpListToolsResult = self
                .call("tools/list", Some(params), LIST_TOOLS_TIMEOUT)
                .await?;

            all_tools.extend(result.tools);

            pages += 1;
            if all_tools.len() > MAX_TOOLS {
                // The page count was bounded; the list they accumulate into
                // was not.
                return Err(McpClientError::TooManyTools { limit: MAX_TOOLS });
            }

            match result.next_cursor {
                Some(next) if !next.is_empty() => {
                    // Checked only when another page is actually asked for, so
                    // a list exactly `MAX_TOOL_PAGES` long is accepted rather
                    // than refused for reaching the limit it may reach.
                    // Cursors are opaque, so a repeat cannot be detected by
                    // value; only this bound stops an endless walk.
                    if pages >= MAX_TOOL_PAGES {
                        return Err(McpClientError::TooManyToolPages {
                            limit: MAX_TOOL_PAGES,
                        });
                    }
                    cursor = Some(next);
                }
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
        self.call_tool_with_timeout(tool_name, arguments, CALL_TOOL_TIMEOUT)
            .await
    }

    /// Call a tool on this server, bounding the wait explicitly.
    pub async fn call_tool_with_timeout(
        &self,
        tool_name: &str,
        arguments: Option<JsonValue>,
        timeout: Duration,
    ) -> Result<McpToolCallResult, McpClientError> {
        let params = McpToolCallParams {
            name: tool_name.to_string(),
            arguments,
        };
        self.call("tools/call", Some(params), timeout).await
    }

    /// The number of requests still awaiting a response.
    #[cfg(test)]
    pub(crate) async fn pending_len(&self) -> usize {
        self.pending.lock().await.len()
    }

    #[cfg(test)]
    pub(crate) async fn drains_stderr(&self) -> bool {
        self.child.lock().await.drains_stderr()
    }

    /// Shut down the MCP server process gracefully.
    pub async fn shutdown(&self) {
        // Best-effort: drop stdin to signal the child.
        let mut stdin = self.stdin.lock().await;
        drop(stdin.shutdown().await);
        drop(stdin);

        // Closing stdin lets a cooperative server finish; terminating the
        // supervised child also handles a server (or descendant) that ignores
        // EOF. The process wrapper kills the whole session/tree.
        let mut child = self.child.lock().await;
        drop(child.terminate().await);
    }
}

/// Reads one newline-delimited frame while retaining at most `limit` bytes.
/// The caller can then drop the client and kill its process tree instead of
/// attempting to recover a protocol stream whose framing is no longer
/// trustworthy.
async fn read_bounded_line<R>(reader: &mut R, limit: usize) -> std::io::Result<Option<Vec<u8>>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut line = Vec::with_capacity(limit.min(8192));
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(line))
            };
        }

        let (take, has_newline) = match chunk.iter().position(|byte| *byte == b'\n') {
            Some(index) => (index + 1, true),
            None => (chunk.len(), false),
        };
        if line.len().saturating_add(take) > limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "MCP stdio frame exceeded its byte limit",
            ));
        }
        line.extend_from_slice(&chunk[..take]);
        reader.consume(take);
        if has_newline {
            return Ok(Some(line));
        }
    }
}
