//! Tests for MCP registration through the runtime builder.

use serde_json::json;

use crate::mcp::sse::testing::SseTestServer;
use crate::mcp::{McpManager, McpSseServerConfig, mcp_tool_name};

const REMOTE_CANARY: &str = "REMOTE_CANARY_MUST_NOT_SURFACE";

/// Scripts a fixture through the handshake, advertising the given tools.
///
/// Returns the manager alongside the bridged tools: the manager owns the
/// connection those tools call through, so dropping it would close the stream.
async fn connect_sse(
    server: &SseTestServer,
    tools: serde_json::Value,
) -> (Vec<crate::mcp::McpBridgedTool>, McpManager) {
    let config = McpSseServerConfig::new("obs", server.sse_url());
    let connecting = tokio::spawn(async move {
        let mut manager = McpManager::new();
        let bridged = manager.connect_sse(&config).await;
        (bridged, manager)
    });

    server.wait_for_stream();
    server.send_endpoint("/messages/?session_id=abc");

    server.wait_for_posts(1);
    server.send_message(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fixture", "version": "4.5.6"}
        }
    }));

    server.wait_for_posts(3);
    server.send_message(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {"tools": tools}
    }));

    let (bridged, manager) = connecting.await.expect("no panic");
    (bridged.expect("the handshake should succeed"), manager)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_manager_bridges_sse_tools_under_a_namespaced_name() {
    let server = SseTestServer::start();
    let (bridged, _manager) = connect_sse(
        &server,
        json!([
            {"name": "search_logs", "description": "Search logs", "inputSchema": {"type": "object"}},
            {"name": "list_alerts", "inputSchema": {"type": "object"}}
        ]),
    )
    .await;

    use crate::tool::ToolDefinition;
    let names: Vec<String> = bridged
        .iter()
        .map(|tool| tool.descriptor().name.to_string())
        .collect();

    assert_eq!(
        names,
        vec![
            mcp_tool_name("obs", "search_logs"),
            mcp_tool_name("obs", "list_alerts"),
        ],
        "SSE tools must be namespaced exactly like stdio tools"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bridged_sse_tool_carries_its_description_and_schema() {
    let server = SseTestServer::start();
    let (bridged, _manager) = connect_sse(
        &server,
        json!([{
            "name": "search_logs",
            "description": "Search the log corpus",
            "inputSchema": {
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"]
            }
        }]),
    )
    .await;

    use crate::tool::ToolDefinition;
    let descriptor = bridged[0].descriptor();
    assert_eq!(
        descriptor.description.as_deref(),
        Some("Search the log corpus")
    );
    assert_eq!(
        descriptor.input_schema["properties"]["query"]["type"], "string",
        "the server's schema must reach the model unchanged"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_manager_reports_a_connected_sse_server() {
    let server = SseTestServer::start();
    let config = McpSseServerConfig::new("obs", server.sse_url());
    let connecting = tokio::spawn(async move {
        let mut manager = McpManager::new();
        let result = manager.connect_sse(&config).await;
        (result.map(|tools| tools.len()), manager)
    });

    server.wait_for_stream();
    server.send_endpoint("/messages/?session_id=abc");
    server.wait_for_posts(1);
    server.send_message(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "serverInfo": {"name": "fixture", "version": "4.5.6"}
        }
    }));
    server.wait_for_posts(3);
    server.send_message(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {"tools": [{"name": "search", "inputSchema": {"type": "object"}}]}
    }));

    let (count, manager) = connecting.await.expect("no panic");
    assert_eq!(count.expect("the handshake should succeed"), 1);

    assert!(manager.is_connected("obs"));
    assert_eq!(manager.connected_count(), 1);
    assert_eq!(
        manager.all_tool_names(),
        vec![mcp_tool_name("obs", "search")]
    );

    let summary = manager
        .list_servers()
        .into_iter()
        .find(|summary| summary.name == "obs")
        .expect("the server should be listed");
    assert_eq!(summary.status, crate::mcp::McpServerStatus::Connected);
    assert_eq!(summary.server_version.as_deref(), Some("4.5.6"));
    assert_eq!(summary.tool_count, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_sse_connection_is_recorded_as_an_error() {
    let server = SseTestServer::with_opening(crate::mcp::sse::testing::StreamOpening::Status {
        code: 404,
        body: "no such stream".to_string(),
    });

    let mut manager = McpManager::new();
    let config = McpSseServerConfig::new("obs", server.sse_url());
    manager
        .connect_sse(&config)
        .await
        .expect_err("a 404 must not connect");

    assert!(!manager.is_connected("obs"));
    let summary = manager
        .list_servers()
        .into_iter()
        .find(|summary| summary.name == "obs")
        .expect("an errored server should still be listed");
    assert_eq!(summary.status, crate::mcp::McpServerStatus::Error);
    assert!(summary.error.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn manager_error_summaries_do_not_retain_json_rpc_text() {
    let server = SseTestServer::start();
    let config = McpSseServerConfig::new("obs", server.sse_url());
    let connecting = tokio::spawn(async move {
        let mut manager = McpManager::new();
        let error = manager
            .connect_sse(&config)
            .await
            .expect_err("the initialize error must fail the connection");
        (error, manager)
    });

    server.wait_for_stream();
    server.send_endpoint("/messages/?session_id=abc");
    server.wait_for_posts(1);
    server.send_message(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": {
            "code": -32001,
            "message": REMOTE_CANARY,
            "data": {"forged": REMOTE_CANARY}
        }
    }));

    let (error, manager) = connecting.await.expect("no panic");
    assert!(
        matches!(error, crate::mcp::McpSseError::JsonRpc(ref rpc) if rpc.code == -32001),
        "got {error:?}"
    );

    let summary = manager
        .list_servers()
        .into_iter()
        .find(|summary| summary.name == "obs")
        .expect("the failed server should be listed");
    let rendered = format!("{summary:?}");
    assert!(!rendered.contains(REMOTE_CANARY), "got {rendered}");
    assert!(
        summary
            .error
            .as_deref()
            .is_some_and(|message| message.contains("-32001")),
        "the safe summary should preserve the JSON-RPC code: {rendered}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_misconfigured_sse_server_fails_before_any_connection_is_opened() {
    let mut manager = McpManager::new();
    // A token over plaintext to a non-loopback host is refused at validation.
    let config =
        McpSseServerConfig::new("obs", "http://internal.corp/sse").with_bearer_token("secret");

    let error = manager
        .connect_sse(&config)
        .await
        .expect_err("validation must reject this before dialing");
    assert!(
        !error.to_string().contains("secret"),
        "the error must not echo the credential: {error}"
    );
}

/// `build_async` must actually register the MCP tools it connects, and a
/// runtime built without any MCP server must not gain namespaced tools.
///
/// Without this, disabling the whole registration arm in `build_async` leaves
/// the suite green: the runtime still builds, it just silently advertises
/// nothing. That failure is invisible until an agent cannot find its tools.
#[tokio::test(flavor = "multi_thread")]
async fn build_async_registers_the_tools_of_a_connected_sse_server() {
    use crate::Runtime;

    let server = SseTestServer::start();
    let config = McpSseServerConfig::new("obs", server.sse_url());

    let building = tokio::spawn(async move {
        Runtime::empty_builder()
            .with_provider_instance(StubProvider)
            .with_mcp_sse_server(config)
            .build_async()
            .await
    });

    server.wait_for_stream();
    server.send_endpoint("/messages/?session_id=abc");
    server.wait_for_posts(1);
    server.send_message(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "serverInfo": {"name": "fixture", "version": "4.5.6"}
        }
    }));
    server.wait_for_posts(3);
    server.send_message(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {"tools": [
            {"name": "search_logs", "inputSchema": {"type": "object"}},
            {"name": "list_alerts", "inputSchema": {"type": "object"}}
        ]}
    }));

    let runtime = building
        .await
        .expect("no panic")
        .expect("the runtime should build");

    let registered: Vec<String> = runtime
        .tools()
        .into_iter()
        .map(|tool| tool.name.to_string())
        .filter(|name| name.starts_with("mcp__"))
        .collect();

    assert_eq!(
        registered.len(),
        2,
        "build_async must register every tool the server advertised, got {registered:?}"
    );
    assert!(registered.contains(&mcp_tool_name("obs", "search_logs")));
    assert!(registered.contains(&mcp_tool_name("obs", "list_alerts")));

    assert!(
        runtime
            .tool_descriptor(&mcp_tool_name("obs", "search_logs"))
            .is_some(),
        "a registered MCP tool must be resolvable by name"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn build_async_registers_no_mcp_tools_without_a_configured_server() {
    use crate::Runtime;

    let runtime = Runtime::empty_builder()
        .with_provider_instance(StubProvider)
        .build_async()
        .await
        .expect("the runtime should build");

    let registered: Vec<String> = runtime
        .tools()
        .into_iter()
        .map(|tool| tool.name.to_string())
        .filter(|name| name.starts_with("mcp__"))
        .collect();

    assert!(
        registered.is_empty(),
        "no MCP server was configured, got {registered:?}"
    );
}

/// A provider that satisfies the builder's "at least one provider" check.
///
/// The registration tests never send a request, so it only needs to exist.
#[derive(Clone)]
struct StubProvider;

#[async_trait::async_trait]
impl crate::provider::Provider for StubProvider {
    fn descriptor(&self) -> crate::provider::ProviderDescriptor {
        crate::provider::ProviderDescriptor::new(crate::BuiltinProvider::Anthropic)
    }

    async fn list_models(&self) -> Result<Vec<crate::ModelInfo>, crate::provider::ProviderError> {
        Ok(vec![crate::ModelInfo::new(
            "stub-model",
            crate::BuiltinProvider::Anthropic,
        )])
    }

    async fn stream(
        &self,
        _request: crate::provider::Request<'_>,
    ) -> Result<crate::provider::ProviderEventStream, crate::provider::ProviderError> {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Ok(rx)
    }
}

/// An SSE-backed tool must reach the model through exactly the same result
/// limiter and paging path as a stdio tool or a custom tool. This runs a real
/// fixture server behind a bridged tool inside a scripted runtime and compares
/// its transcript entry byte for byte against a custom tool returning the same
/// text.
#[tokio::test(flavor = "multi_thread")]
async fn sse_tool_output_is_limited_exactly_like_a_custom_tool() {
    use std::collections::BTreeMap;

    use crate::{
        ContentBlock,
        runtime::RuntimePolicy,
        test::{MockRuntime, MockToolCall},
        tool::ToolDefinition,
    };

    let full_output = "one\ntwo\nthree";
    let server = SseTestServer::start();
    let (bridged, _manager) = connect_sse(
        &server,
        json!([{"name": "large_output", "inputSchema": {"type": "object"}}]),
    )
    .await;
    let bridged_name = bridged[0].descriptor().name.to_string();

    let mock = MockRuntime::builder()
        .with_policy(
            RuntimePolicy::permissive()
                .with_max_tool_result_bytes(8)
                .with_max_tool_result_lines(1)
                .spill_full_tool_output(false),
        )
        .tool_calls([
            MockToolCall::new(&bridged_name, json!({})).with_id("sse-call"),
            MockToolCall::new("matching_custom_output", json!({})).with_id("custom-call"),
        ])
        .text("done")
        .build()
        .expect("build mock runtime");

    for tool in bridged {
        mock.runtime().register_tool(tool);
    }
    mock.runtime().register_tool(EchoTool {
        output: full_output.to_string(),
    });

    let mut agent = mock
        .runtime()
        .spawn("mcp-sse-truncation-test", mock.model())
        .expect("spawn agent");

    let running = tokio::spawn(async move {
        let response = agent
            .send(vec![ContentBlock::text("run both tools")])
            .await
            .expect("run agent");
        (response, agent)
    });

    // Answer the bridged tool call once the runtime has posted it.
    server.wait_for_posts(4);
    server.send_message(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "result": {
            "content": [{"type": "text", "text": full_output}],
            "isError": false
        }
    }));

    let (response, _agent) = running.await.expect("no panic");
    assert_eq!(response.text(), "done");

    let requests = mock.recorded_requests().await;
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

    let sse_result = provider_results
        .get("sse-call")
        .expect("the SSE tool result should reach the provider");
    let custom_result = provider_results
        .get("custom-call")
        .expect("the custom tool result should reach the provider");

    assert_eq!(
        sse_result, custom_result,
        "an SSE tool must be limited identically to any other tool"
    );
    assert_eq!(
        *sse_result,
        (
            "one\n[truncated: showing 1 of 3 lines; full output was not saved because spill-to-file is disabled by runtime policy]",
            false,
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bridged_sse_errors_do_not_put_json_rpc_text_in_model_context() {
    use crate::{
        ContentBlock,
        test::{MockRuntime, MockToolCall},
        tool::ToolDefinition,
    };

    let server = SseTestServer::start();
    let (bridged, _manager) = connect_sse(
        &server,
        json!([{"name": "fail", "inputSchema": {"type": "object"}}]),
    )
    .await;
    let bridged_name = bridged[0].descriptor().name.to_string();

    let mock = MockRuntime::builder()
        .tool_calls([MockToolCall::new(&bridged_name, json!({})).with_id("sse-error")])
        .text("done")
        .build()
        .expect("build mock runtime");
    for tool in bridged {
        mock.runtime().register_tool(tool);
    }

    let mut agent = mock
        .runtime()
        .spawn("mcp-sse-error-redaction-test", mock.model())
        .expect("spawn agent");
    let running = tokio::spawn(async move {
        agent
            .send(vec![ContentBlock::text("call the failing tool")])
            .await
    });

    server.wait_for_posts(4);
    server.send_message(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "error": {
            "code": -32602,
            "message": REMOTE_CANARY,
            "data": {"forged": REMOTE_CANARY}
        }
    }));

    let response = running
        .await
        .expect("no panic")
        .expect("the agent should continue after the tool error");
    assert_eq!(response.text(), "done");

    let requests = mock.recorded_requests().await;
    let (content, is_error) = requests[1]
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } if tool_use_id == "sse-error" => Some((content.as_str(), *is_error)),
            _ => None,
        })
        .expect("the provider should receive the bridged error");

    assert!(is_error);
    assert!(!content.contains(REMOTE_CANARY), "got {content}");
    assert!(content.contains("-32602"), "got {content}");
}

/// A custom tool returning a fixed string, used as the limiter baseline.
struct EchoTool {
    output: String,
}

impl crate::tool::ToolDefinition for EchoTool {
    fn descriptor(&self) -> crate::tool::ToolSpec {
        crate::tool::ToolSpec::builder("matching_custom_output")
            .description("Return the same output as the MCP test tool")
            .input_schema(json!({ "type": "object", "properties": {} }))
            .side_effect_level(crate::tool::ToolSideEffectLevel::External)
            .build()
    }
}

#[async_trait::async_trait]
impl crate::tool::ToolExecutor for EchoTool {
    async fn execute(
        &self,
        _ctx: crate::tool::ParallelToolContext,
        _input: serde_json::Value,
    ) -> crate::tool::ToolResult {
        Ok(self.output.clone())
    }
}
