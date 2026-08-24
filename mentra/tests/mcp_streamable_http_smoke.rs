//! Optional manual smoke test against a real MCP Streamable HTTP server.
//!
//! This is ignored by default and never runs in ordinary CI: it needs a live
//! endpoint, which no automated run should depend on. The transport itself is
//! covered by the deterministic fixture tests in
//! `mentra::mcp::streamable_http`.
//!
//! Point it at any server speaking the 2025-03-26-or-later Streamable HTTP
//! transport — the single MCP endpoint, commonly `/mcp`:
//!
//! ```text
//! MENTRA_MCP_HTTP_URL=https://mcp.example.com/mcp \
//! MENTRA_MCP_HTTP_TOKEN=<token> \
//!     cargo test -p mentra --test mcp_streamable_http_smoke -- --ignored --nocapture
//! ```
//!
//! `MENTRA_MCP_HTTP_TOKEN` is optional. The test performs only `initialize` and
//! `tools/list`; it never calls a tool, so it cannot cause a side effect on the
//! server it is pointed at.

use mentra::{McpStreamableHttpClient, McpStreamableHttpServerConfig};

/// Environment variable naming the MCP endpoint to probe.
const URL_VAR: &str = "MENTRA_MCP_HTTP_URL";
/// Environment variable carrying an optional bearer token.
const TOKEN_VAR: &str = "MENTRA_MCP_HTTP_TOKEN";

#[tokio::test]
#[ignore = "requires a live MCP server; set MENTRA_MCP_HTTP_URL"]
async fn initializes_and_lists_tools_against_a_live_server() {
    let url = std::env::var(URL_VAR)
        .unwrap_or_else(|_| panic!("set {URL_VAR} to the server's MCP endpoint"));

    let mut config = McpStreamableHttpServerConfig::new("smoke", &url);
    if let Ok(token) = std::env::var(TOKEN_VAR) {
        config = config.with_bearer_token(token);
    }

    let client = McpStreamableHttpClient::connect(&config)
        .await
        .expect("the handshake should complete");

    let info = client
        .server_info()
        .expect("initialize should report server info");
    println!("connected to {} {}", info.name, info.version);

    match client.session_id() {
        Some(session) => println!("server assigned a session ({} chars)", session.len()),
        None => println!("server uses no session"),
    }

    println!("{} tools advertised:", client.tools().len());
    for tool in client.tools() {
        println!("  {}", tool.name);
    }

    // No tool is called: a smoke test must not cause side effects on whatever
    // server it happens to be pointed at.
    client.shutdown().await;
}
