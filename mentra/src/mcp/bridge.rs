//! Bridge that wraps MCP server tools as Mentra `ExecutableTool` instances.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::tool::{
    ParallelToolContext, RuntimeToolDescriptor, ToolApprovalCategory, ToolCapability,
    ToolDefinition, ToolDurability, ToolExecutionCategory, ToolExecutor, ToolResult,
    ToolSideEffectLevel,
};

use super::client::McpStdioClient;
use super::protocol::{McpToolCallResult, McpToolDefinition};
use super::sse::client::McpSseClient;
use super::streamable_http::client::McpStreamableHttpClient;

/// The transport-independent surface [`McpBridgedTool`] needs from a client.
///
/// Each transport reports failures with its own error type, so this trait
/// flattens them to a message rather than forcing a shared error enum on the
/// public clients.
///
/// This is a sealed trait: it is public only so that
/// [`McpBridgedTool::new`] can be generic over the transport, and it is not
/// implementable outside this crate.
#[async_trait]
pub trait McpToolClient: sealed::Sealed + Send + Sync {
    /// Calls one tool, rendering any transport failure as a message.
    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Option<Value>,
    ) -> Result<McpToolCallResult, String>;
}

mod sealed {
    /// Prevents outside implementations of [`super::McpToolClient`].
    pub trait Sealed {}

    impl Sealed for super::McpStdioClient {}
    impl Sealed for super::McpSseClient {}
    impl Sealed for super::McpStreamableHttpClient {}

    #[cfg(test)]
    impl Sealed for crate::mcp::tests::SuccessfulMcpClient {}
}

#[async_trait]
impl McpToolClient for McpStdioClient {
    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Option<Value>,
    ) -> Result<McpToolCallResult, String> {
        McpStdioClient::call_tool(self, tool_name, arguments)
            .await
            .map_err(|error| error.to_string())
    }
}

#[async_trait]
impl McpToolClient for McpSseClient {
    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Option<Value>,
    ) -> Result<McpToolCallResult, String> {
        McpSseClient::call_tool(self, tool_name, arguments)
            .await
            .map_err(|error| error.to_string())
    }
}

#[async_trait]
impl McpToolClient for McpStreamableHttpClient {
    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Option<Value>,
    ) -> Result<McpToolCallResult, String> {
        McpStreamableHttpClient::call_tool(self, tool_name, arguments)
            .await
            .map_err(|error| error.to_string())
    }
}

/// Prefix applied to MCP tool names to namespace them.
const MCP_TOOL_PREFIX: &str = "mcp__";

/// Construct the namespaced tool name for an MCP tool: `mcp__{server}__{tool}`.
///
/// # Grammar
///
/// The result is recovered by [`parse_mcp_tool_name`], which splits the
/// un-prefixed remainder on its *first* `__`. That means `server_name` must
/// be validated with [`validate_mcp_server_name`] before it ever reaches
/// this function — a server name containing `__` or ending in `_` makes two
/// distinct `(server, tool)` pairs collide on the same encoded string. This
/// function does not itself validate `server_name`; callers that accept a
/// server name from configuration must reject it earlier (every entry point
/// on [`McpManager`](super::manager::McpManager) does).
///
/// `tool_name` carries no such restriction: it may contain `__` or start
/// with `_` and still parses back correctly, because the split only looks
/// for the first `__`, which the validated server name cannot itself
/// contain.
pub fn mcp_tool_name(server_name: &str, tool_name: &str) -> String {
    format!("{MCP_TOOL_PREFIX}{server_name}__{tool_name}")
}

/// Parse a namespaced MCP tool name back into `(server_name, tool_name)`.
///
/// Inverts [`mcp_tool_name`] by splitting the un-prefixed remainder on its
/// *first* `__`. This is only unambiguous for names produced from a server
/// name that passed [`validate_mcp_server_name`]; see that function's doc
/// comment for the shapes that would otherwise misparse.
pub fn parse_mcp_tool_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix(MCP_TOOL_PREFIX)?;
    let (server, tool) = rest.split_once("__")?;
    Some((server, tool))
}

/// Errors rejecting an MCP server name whose shape would make
/// [`parse_mcp_tool_name`] misparse a name [`mcp_tool_name`] built from it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum McpServerNameError {
    /// An empty server name is never a meaningful identity for a bridged
    /// tool's owner.
    #[error("MCP server name must not be empty")]
    Empty,
    /// A `__` inside the server name is indistinguishable from the
    /// server/tool separator [`mcp_tool_name`] inserts: server `evil__foo`
    /// with tool `real_tool` encodes to the same `mcp__evil__foo__real_tool`
    /// that [`parse_mcp_tool_name`] splits at the *first* `__`, recovering
    /// server `evil`, tool `foo__real_tool` instead of the true pair.
    #[error(
        "MCP server name {0:?} must not contain \"__\", which mcp_tool_name uses as the \
         separator between the server name and the tool name"
    )]
    ContainsDoubleUnderscore(String),
    /// A trailing `_` merges with the leading `_` of the `__` separator:
    /// server `evil_` with tool `_thing` encodes to the same
    /// `mcp__evil____thing` as server `evil` with tool `__thing`.
    #[error(
        "MCP server name {0:?} must not end with \"_\", which would merge with the \"__\" \
         separator mcp_tool_name inserts after it"
    )]
    EndsWithUnderscore(String),
}

/// Validates a server name before it is ever passed to [`mcp_tool_name`].
///
/// Rejects the three shapes documented on [`McpServerNameError`]: empty,
/// containing `__`, or ending in `_`. Every registration and connect entry
/// point on [`McpManager`](super::manager::McpManager) calls this before
/// bridging a server's tools, so an operator-configured name that would make
/// [`parse_mcp_tool_name`] ambiguous is rejected at connect time rather than
/// silently misattributing tool calls between servers.
pub fn validate_mcp_server_name(server_name: &str) -> Result<(), McpServerNameError> {
    if server_name.is_empty() {
        return Err(McpServerNameError::Empty);
    }
    if server_name.contains("__") {
        return Err(McpServerNameError::ContainsDoubleUnderscore(
            server_name.to_string(),
        ));
    }
    if server_name.ends_with('_') {
        return Err(McpServerNameError::EndsWithUnderscore(
            server_name.to_string(),
        ));
    }
    Ok(())
}

/// A Mentra tool backed by an MCP server tool.
pub struct McpBridgedTool {
    server_name: String,
    tool_def: McpToolDefinition,
    client: Arc<dyn McpToolClient>,
}

impl McpBridgedTool {
    /// Wraps one tool from a connected MCP server.
    ///
    /// The client is generic over the transport, so this accepts an
    /// `Arc<McpStdioClient>` and an `Arc<McpSseClient>` alike.
    pub fn new<C>(server_name: String, tool_def: McpToolDefinition, client: Arc<C>) -> Self
    where
        C: McpToolClient + 'static,
    {
        Self::from_client(server_name, tool_def, client)
    }

    fn from_client(
        server_name: String,
        tool_def: McpToolDefinition,
        client: Arc<dyn McpToolClient>,
    ) -> Self {
        Self {
            server_name,
            tool_def,
            client,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        server_name: String,
        tool_def: McpToolDefinition,
        client: Arc<dyn McpToolClient>,
    ) -> Self {
        Self::from_client(server_name, tool_def, client)
    }

    fn full_name(&self) -> String {
        mcp_tool_name(&self.server_name, &self.tool_def.name)
    }
}

impl std::fmt::Debug for McpBridgedTool {
    /// Renders the bridged identity without reaching into the client, which
    /// holds transport credentials.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpBridgedTool")
            .field("name", &self.full_name())
            .finish_non_exhaustive()
    }
}

impl ToolDefinition for McpBridgedTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        let description = self.tool_def.description.clone().unwrap_or_default();

        let input_schema = self
            .tool_def
            .input_schema
            .clone()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));

        RuntimeToolDescriptor::builder(self.full_name())
            .description(description)
            .input_schema(input_schema)
            .capability(ToolCapability::Custom(format!("mcp:{}", self.server_name)))
            .side_effect_level(ToolSideEffectLevel::External)
            .durability(ToolDurability::Ephemeral)
            .execution_category(ToolExecutionCategory::ExclusiveLocalMutation)
            .approval_category(ToolApprovalCategory::Process)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for McpBridgedTool {
    async fn execute(&self, _ctx: ParallelToolContext, input: Value) -> ToolResult {
        let arguments = if input.is_null()
            || (input.is_object() && input.as_object().is_none_or(|o| o.is_empty()))
        {
            None
        } else {
            Some(input)
        };

        let result = self
            .client
            .call_tool(&self.tool_def.name, arguments)
            .await
            .map_err(|error| format!("MCP tool call failed: {error}"))?;

        // Concatenate text content blocks into the result string.
        let mut output = String::new();
        for block in &result.content {
            if let Some(text) = &block.text {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(text);
            }
        }

        if result.is_error {
            Err(output)
        } else {
            Ok(output)
        }
    }
}

#[cfg(test)]
mod name_tests {
    use super::*;

    #[test]
    fn empty_server_name_is_rejected() {
        assert_eq!(validate_mcp_server_name(""), Err(McpServerNameError::Empty));
    }

    #[test]
    fn server_name_containing_double_underscore_is_rejected() {
        assert_eq!(
            validate_mcp_server_name("evil__foo"),
            Err(McpServerNameError::ContainsDoubleUnderscore(
                "evil__foo".to_string()
            ))
        );
    }

    #[test]
    fn server_name_ending_in_underscore_is_rejected() {
        assert_eq!(
            validate_mcp_server_name("evil_"),
            Err(McpServerNameError::EndsWithUnderscore("evil_".to_string()))
        );
    }

    #[test]
    fn ordinary_server_name_is_accepted() {
        assert_eq!(validate_mcp_server_name("evil"), Ok(()));
    }

    #[test]
    fn colliding_names_from_the_issue_no_longer_collide() {
        // Before validation, server `evil__foo` tool `real_tool` and server
        // `evil` tool `foo__real_tool` both encoded to
        // `mcp__evil__foo__real_tool`. Rejecting the first server name at
        // the source removes the collision.
        assert!(validate_mcp_server_name("evil__foo").is_err());
        assert_eq!(
            parse_mcp_tool_name(&mcp_tool_name("evil", "foo__real_tool")),
            Some(("evil", "foo__real_tool"))
        );

        // Before validation, server `evil_` tool `_thing` and server `evil`
        // tool `__thing` both encoded to `mcp__evil____thing`.
        assert!(validate_mcp_server_name("evil_").is_err());
        assert_eq!(
            parse_mcp_tool_name(&mcp_tool_name("evil", "__thing")),
            Some(("evil", "__thing"))
        );
    }

    #[test]
    fn round_trip_preserves_tool_names_containing_double_underscore_or_leading_underscore() {
        for (server, tool) in [
            ("obs", "search__logs"),
            ("obs", "__internal"),
            ("obs", "logs"),
        ] {
            validate_mcp_server_name(server).expect("valid server name");
            let encoded = mcp_tool_name(server, tool);
            assert_eq!(parse_mcp_tool_name(&encoded), Some((server, tool)));
        }
    }
}
