//! Connects to an MCP server over the legacy HTTP+SSE transport.
//!
//! Two ways to use the transport are shown:
//!
//! 1. `McpSseClient` directly, for a host that wants to inspect what a server
//!    advertises and choose what to call.
//! 2. `RuntimeBuilder::with_mcp_sse_server`, which bridges every advertised
//!    tool into the runtime under a namespaced name.
//!
//! Run it against any server speaking the 2024-11-05 HTTP+SSE transport:
//!
//! ```text
//! MCP_SSE_URL=https://mcp.example.com/sse \
//! MCP_SSE_TOKEN=<token> \
//!     cargo run -p mentra-examples --example mcp_sse
//! ```
//!
//! `MCP_SSE_TOKEN` is optional. Set `ANTHROPIC_API_KEY` as well to also build a
//! runtime with the server's tools registered.

use mentra::{BuiltinProvider, McpSseClient, McpSseServerConfig, Runtime};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();

    let url = match std::env::var("MCP_SSE_URL") {
        Ok(url) => url,
        Err(_) => {
            eprintln!("Set MCP_SSE_URL to the server's SSE endpoint, for example");
            eprintln!("  MCP_SSE_URL=https://mcp.example.com/sse");
            return Ok(());
        }
    };

    // Headers are stored as secrets: they are sent on both the stream and every
    // POST, and never appear in Debug output, errors, or logs.
    let mut config = McpSseServerConfig::new("example", &url);
    if let Ok(token) = std::env::var("MCP_SSE_TOKEN") {
        config = config.with_bearer_token(token);
    }

    // --- 1. Drive the client directly -------------------------------------
    //
    // Nothing is registered with a runtime here, so a host can apply its own
    // allowlist or redaction before deciding what the model may reach.
    println!("Connecting to {url} ...");
    let client = McpSseClient::connect(&config).await?;

    if let Some(info) = client.server_info() {
        println!("Connected to {} {}", info.name, info.version);
    }

    println!("\n{} tools advertised:", client.tools().len());
    for tool in client.tools() {
        let description = tool.description.as_deref().unwrap_or("(no description)");
        println!("  {} — {description}", tool.name);
    }

    // Call the first tool that takes no required arguments, if there is one.
    if let Some(tool) = client.tools().iter().find(|tool| !requires_arguments(tool)) {
        println!("\nCalling {} ...", tool.name);
        match client.call_tool(&tool.name, None).await {
            Ok(result) => {
                let text: Vec<&str> = result
                    .content
                    .iter()
                    .filter_map(|block| block.text.as_deref())
                    .collect();
                println!("  is_error: {}", result.is_error);
                println!("  {}", text.join("\n  "));
            }
            // A failed call may still have executed: the POST and the response
            // travel on different connections, so an unanswered call is
            // reported as indeterminate rather than retried.
            Err(error) => println!("  call failed: {error}"),
        }
    }

    client.shutdown().await;

    // --- 2. Bridge the tools into a runtime -------------------------------
    let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") else {
        println!("\nSet ANTHROPIC_API_KEY to also build a runtime with these tools.");
        return Ok(());
    };

    let runtime = Runtime::builder()
        .with_provider(BuiltinProvider::Anthropic, api_key)
        .with_mcp_sse_server(config)
        .build_async()
        .await?;

    println!("\nRuntime built with the server's tools registered as mcp__example__*.");
    let _ = runtime;

    Ok(())
}

/// Reports whether a tool's schema marks any property as required.
fn requires_arguments(tool: &mentra::mcp::McpToolDefinition) -> bool {
    tool.input_schema
        .as_ref()
        .and_then(|schema| schema.get("required"))
        .and_then(|required| required.as_array())
        .is_some_and(|required| !required.is_empty())
}
