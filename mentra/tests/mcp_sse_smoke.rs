//! Optional manual smoke test against a real MCP HTTP+SSE server.
//!
//! This is ignored by default and never runs in ordinary CI: it needs a live
//! endpoint, which no automated run should depend on. The transport itself is
//! covered by the deterministic fixture tests in `mentra::mcp::sse`.
//!
//! Point it at any server speaking the 2024-11-05 HTTP+SSE transport:
//!
//! ```text
//! MENTRA_MCP_SSE_URL=https://mcp.example.com/sse \
//! MENTRA_MCP_SSE_TOKEN=<token> \
//!     cargo test -p mentra --test mcp_sse_smoke -- --ignored --nocapture
//! ```
//!
//! `MENTRA_MCP_SSE_TOKEN` is optional. The test performs only `initialize` and
//! `tools/list`; it never calls a tool, so it cannot cause a side effect on the
//! server it is pointed at.

use mentra::{McpSseClient, McpSseServerConfig};

/// Environment variable naming the SSE endpoint to probe.
const URL_VAR: &str = "MENTRA_MCP_SSE_URL";
/// Environment variable carrying an optional bearer token.
const TOKEN_VAR: &str = "MENTRA_MCP_SSE_TOKEN";

#[tokio::test]
#[ignore = "requires a live MCP server; set MENTRA_MCP_SSE_URL"]
async fn initializes_and_lists_tools_against_a_live_server() {
    let url = std::env::var(URL_VAR)
        .unwrap_or_else(|_| panic!("set {URL_VAR} to the server's SSE endpoint"));

    let mut config = McpSseServerConfig::new("smoke", &url);
    if let Ok(token) = std::env::var(TOKEN_VAR) {
        config = config.with_bearer_token(token);
    }

    let client = McpSseClient::connect(&config)
        .await
        .expect("the handshake should complete");

    let info = client
        .server_info()
        .expect("initialize should report server info");
    println!("connected to {} {}", info.name, info.version);

    println!("{} tools advertised:", client.tools().len());
    for tool in client.tools() {
        println!("  {}", tool.name);
    }

    // No tool is called: a smoke test must not cause side effects on whatever
    // server it happens to be pointed at.
    client.shutdown().await;
}
