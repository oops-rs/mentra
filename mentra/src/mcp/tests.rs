use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{
    ContentBlock,
    mcp::{
        bridge::{McpBridgedTool, McpToolClient, mcp_tool_name, parse_mcp_tool_name},
        client::McpClientError,
        protocol::*,
    },
    runtime::RuntimePolicy,
    test::{MockRuntime, MockToolCall},
    tool::{
        ParallelToolContext, ToolDefinition, ToolExecutor, ToolResult, ToolSideEffectLevel,
        ToolSpec,
    },
};

struct SuccessfulMcpClient {
    output: String,
}

#[async_trait]
impl McpToolClient for SuccessfulMcpClient {
    async fn call_tool(
        &self,
        _tool_name: &str,
        _arguments: Option<Value>,
    ) -> Result<McpToolCallResult, McpClientError> {
        Ok(McpToolCallResult {
            content: vec![McpToolCallContent {
                kind: "text".to_string(),
                text: Some(self.output.clone()),
                data: None,
                mime_type: None,
            }],
            is_error: false,
        })
    }
}

struct MatchingCustomTool {
    output: String,
}

impl ToolDefinition for MatchingCustomTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("matching_custom_output")
            .description("Return the same output as the MCP test tool")
            .input_schema(json!({ "type": "object", "properties": {} }))
            .side_effect_level(ToolSideEffectLevel::External)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for MatchingCustomTool {
    async fn execute(&self, _ctx: ParallelToolContext, _input: Value) -> ToolResult {
        Ok(self.output.clone())
    }
}

#[test]
fn mcp_tool_name_namespacing() {
    assert_eq!(mcp_tool_name("filesystem", "read"), "mcp__filesystem__read");
    assert_eq!(
        mcp_tool_name("my-server", "do_thing"),
        "mcp__my-server__do_thing"
    );
}

#[test]
fn parse_mcp_tool_name_roundtrip() {
    let name = mcp_tool_name("filesystem", "read");
    let (server, tool) = parse_mcp_tool_name(&name).expect("should parse");
    assert_eq!(server, "filesystem");
    assert_eq!(tool, "read");
}

#[test]
fn parse_mcp_tool_name_rejects_non_mcp() {
    assert!(parse_mcp_tool_name("regular_tool").is_none());
    assert!(parse_mcp_tool_name("mcp_no_double_underscore").is_none());
}

#[tokio::test]
async fn bridged_output_is_truncated_before_the_next_provider_request() {
    let full_output = "one\ntwo\nthree";
    let bridged_name = mcp_tool_name("fake", "large_output");
    let mock = MockRuntime::builder()
        .with_policy(
            RuntimePolicy::permissive()
                .with_max_tool_result_bytes(8)
                .with_max_tool_result_lines(1)
                .spill_full_tool_output(false),
        )
        .tool_calls([
            MockToolCall::new(&bridged_name, json!({})).with_id("mcp-call"),
            MockToolCall::new("matching_custom_output", json!({})).with_id("custom-call"),
        ])
        .text("done")
        .build()
        .expect("build mock runtime");
    mock.runtime().register_tool(McpBridgedTool::new_for_test(
        "fake".to_string(),
        McpToolDefinition {
            name: "large_output".to_string(),
            description: Some("Return an oversized result".to_string()),
            input_schema: Some(json!({ "type": "object", "properties": {} })),
        },
        Arc::new(SuccessfulMcpClient {
            output: full_output.to_string(),
        }),
    ));
    mock.runtime().register_tool(MatchingCustomTool {
        output: full_output.to_string(),
    });
    let mut agent = mock
        .runtime()
        .spawn("mcp-truncation-test", mock.model())
        .expect("spawn agent");

    let response = agent
        .send(vec![ContentBlock::text("run both tools")])
        .await
        .expect("run agent");
    assert_eq!(response.text(), "done");

    let requests = mock.recorded_requests().await;
    assert_eq!(requests.len(), 2);
    let provider_results = requests[1]
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some((tool_use_id.as_str(), (content.as_str(), *is_error))),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let mcp_result = provider_results
        .get("mcp-call")
        .expect("provider request should contain the MCP result");
    let custom_result = provider_results
        .get("custom-call")
        .expect("provider request should contain the custom-tool result");
    assert_eq!(mcp_result, custom_result);
    assert_eq!(
        *mcp_result,
        (
            "one\n[truncated: showing 1 of 3 lines; full output was not saved because spill-to-file is disabled by runtime policy]",
            false,
        )
    );
}

#[test]
fn json_rpc_request_serialization() {
    let req = JsonRpcRequest::new(1, "initialize", Some(json!({"key": "value"})));
    let serialized = serde_json::to_string(&req).expect("serialize");
    assert!(serialized.contains("\"jsonrpc\":\"2.0\""));
    assert!(serialized.contains("\"id\":1"));
    assert!(serialized.contains("\"method\":\"initialize\""));
}

#[test]
fn json_rpc_response_deserialization() {
    let json = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
    let resp: JsonRpcResponse = serde_json::from_str(json).expect("deserialize");
    assert_eq!(resp.id, JsonRpcId::Number(1));
    assert!(resp.result.is_some());
    assert!(resp.error.is_none());
}

#[test]
fn json_rpc_error_response_deserialization() {
    let json = r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32600,"message":"Invalid Request"}}"#;
    let resp: JsonRpcResponse = serde_json::from_str(json).expect("deserialize");
    assert_eq!(resp.id, JsonRpcId::Number(2));
    let err = resp.error.expect("should have error");
    assert_eq!(err.code, -32600);
    assert_eq!(err.message, "Invalid Request");
}

#[test]
fn mcp_tool_definition_deserialization() {
    let json = json!({
        "name": "read_file",
        "description": "Read a file from disk",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": {"type": "string"}
            },
            "required": ["path"]
        }
    });
    let tool: McpToolDefinition = serde_json::from_value(json).expect("deserialize");
    assert_eq!(tool.name, "read_file");
    assert_eq!(tool.description.as_deref(), Some("Read a file from disk"));
    assert!(tool.input_schema.is_some());
}

#[test]
fn mcp_tool_call_result_deserialization() {
    let json = json!({
        "content": [
            {"type": "text", "text": "Hello, world!"},
            {"type": "text", "text": "Second block"}
        ],
        "isError": false
    });
    let result: McpToolCallResult = serde_json::from_value(json).expect("deserialize");
    assert_eq!(result.content.len(), 2);
    assert!(!result.is_error);
    assert_eq!(result.content[0].text.as_deref(), Some("Hello, world!"));
}

#[test]
fn mcp_tool_call_error_result() {
    let json = json!({
        "content": [{"type": "text", "text": "Something went wrong"}],
        "isError": true
    });
    let result: McpToolCallResult = serde_json::from_value(json).expect("deserialize");
    assert!(result.is_error);
}

#[test]
fn mcp_server_config_deserialization() {
    let json = json!({
        "name": "filesystem",
        "command": "npx",
        "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
        "env": {"DEBUG": "1"},
        "cwd": "/home/user"
    });
    let config: McpServerConfig = serde_json::from_value(json).expect("deserialize");
    assert_eq!(config.name, "filesystem");
    assert_eq!(config.command, "npx");
    assert_eq!(config.args.len(), 3);
    assert_eq!(config.env.get("DEBUG").map(String::as_str), Some("1"));
    assert_eq!(config.cwd.as_deref(), Some("/home/user"));
}

#[test]
fn mcp_initialize_params_serialization() {
    let params = McpInitializeParams {
        protocol_version: "2024-11-05".to_string(),
        capabilities: json!({}),
        client_info: McpClientInfo {
            name: "mentra".to_string(),
            version: "0.6.0".to_string(),
        },
    };
    let json = serde_json::to_value(&params).expect("serialize");
    assert_eq!(json["protocolVersion"], "2024-11-05");
    assert_eq!(json["clientInfo"]["name"], "mentra");
}
