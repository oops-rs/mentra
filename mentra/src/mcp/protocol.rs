//! JSON-RPC 2.0 and MCP protocol types.

#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 primitives
// ---------------------------------------------------------------------------

/// JSON-RPC 2.0 request identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    Number(u64),
    String(String),
    Null,
}

/// Outbound JSON-RPC 2.0 request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest<T = JsonValue> {
    pub jsonrpc: String,
    pub id: JsonRpcId,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<T>,
}

impl<T> JsonRpcRequest<T> {
    pub fn new(id: u64, method: impl Into<String>, params: Option<T>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: JsonRpcId::Number(id),
            method: method.into(),
            params,
        }
    }
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<JsonValue>,
}

impl std::fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON-RPC error {}: {}", self.code, self.message)
    }
}

/// Inbound JSON-RPC 2.0 response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse<T = JsonValue> {
    pub jsonrpc: String,
    pub id: JsonRpcId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

// ---------------------------------------------------------------------------
// MCP initialize
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpInitializeParams {
    pub protocol_version: String,
    pub capabilities: JsonValue,
    pub client_info: McpClientInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpClientInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpInitializeResult {
    pub protocol_version: String,
    pub capabilities: JsonValue,
    pub server_info: McpServerInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerInfo {
    pub name: String,
    #[serde(default)]
    pub version: String,
}

// ---------------------------------------------------------------------------
// MCP tools/list
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpListToolsParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// A tool definition returned by the MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<JsonValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpListToolsResult {
    pub tools: Vec<McpToolDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

// ---------------------------------------------------------------------------
// MCP tools/call
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolCallParams {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<JsonValue>,
}

/// A single content block in a tool call result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolCallContent {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolCallResult {
    pub content: Vec<McpToolCallContent>,
    #[serde(default)]
    pub is_error: bool,
}

// ---------------------------------------------------------------------------
// MCP server configuration
// ---------------------------------------------------------------------------

/// Configuration for a single MCP server.
///
/// # Security
///
/// [`env`](Self::env) is where the de-facto `.mcp.json` format puts server
/// credentials, so its values never reach `Debug` output: the rendering keeps
/// the variable names, which is what an operator needs to see, and replaces
/// every value. [`Serialize`] is unaffected — a config written back to disk
/// still carries the values it was given.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Display name for the server.
    pub name: String,
    /// The command to spawn.
    pub command: String,
    /// Arguments to pass to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables.
    ///
    /// Values are redacted by `Debug`; see the type's security note.
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// Working directory for the spawned process.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// Renders every field except the environment values.
///
/// Written by hand rather than derived so that formatting a config for a log,
/// an error, or a panic cannot print a token. The remaining fields stay visible
/// because `command` and `args` are how an operator identifies which server the
/// configuration describes.
impl std::fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServerConfig")
            .field("name", &self.name)
            .field("command", &self.command)
            .field("args", &self.args)
            .field("env", &RedactedEnv(&self.env))
            .field("cwd", &self.cwd)
            .finish()
    }
}

/// Renders environment variable names with their values replaced.
struct RedactedEnv<'a>(&'a std::collections::HashMap<String, String>);

impl std::fmt::Debug for RedactedEnv<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Sorted because `HashMap` iteration order varies between processes,
        // and a diagnostic that reorders itself between runs is harder to read
        // and harder to diff.
        let mut names: Vec<&str> = self.0.keys().map(String::as_str).collect();
        names.sort_unstable();

        let mut map = f.debug_map();
        for name in names {
            map.entry(&name, &Redacted);
        }
        map.finish()
    }
}

/// Stands in for a value that must not be printed.
struct Redacted;

impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}
