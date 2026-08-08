//! Tests for the MCP stdio client, driven by a scripted server process.
//!
//! The server is a short Python program so the test can control exactly which
//! JSON-RPC frames come back, including ones a well-behaved server would never
//! send. Tests that need it are skipped when no interpreter is available rather
//! than failing, so the suite still runs on a machine without Python.

use std::collections::HashMap;
use std::time::Duration;

use super::{McpClientError, McpStdioClient};
use crate::mcp::protocol::McpServerConfig;

/// Returns an interpreter that can run the scripted server, if one exists.
fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|candidate| {
        std::process::Command::new(candidate)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// Builds a config running the given Python source as an MCP server.
fn scripted_server(python: &str, source: &str) -> McpServerConfig {
    McpServerConfig {
        name: "scripted".to_string(),
        command: python.to_string(),
        args: vec!["-c".to_string(), source.to_string()],
        env: HashMap::new(),
        cwd: None,
    }
}

/// A server that completes the handshake, then behaves as `extra` directs.
///
/// `extra` runs after `tools/list`, receiving each subsequent request line.
fn handshake_server(extra: &str) -> String {
    format!(
        r#"
import sys, json

def send(payload):
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()

def read():
    line = sys.stdin.readline()
    if not line:
        raise SystemExit(0)
    return json.loads(line)

# initialize
request = read()
send({{"jsonrpc": "2.0", "id": request["id"], "result": {{
    "protocolVersion": "2024-11-05",
    "capabilities": {{}},
    "serverInfo": {{"name": "scripted", "version": "9.9.9"}}}}}})

# notifications/initialized carries no id and expects no reply
read()

# tools/list
request = read()
send({{"jsonrpc": "2.0", "id": request["id"], "result": {{"tools": [
    {{"name": "echo", "inputSchema": {{"type": "object"}}}}]}}}})

{extra}
"#
    )
}

#[tokio::test]
async fn completes_the_handshake_and_discovers_tools() {
    let Some(python) = python() else {
        eprintln!("skipping: no Python interpreter available");
        return;
    };

    let config = scripted_server(python, &handshake_server("read()"));
    let client = McpStdioClient::connect(&config)
        .await
        .expect("the handshake should succeed");

    assert_eq!(
        client.server_info().map(|info| info.name.as_str()),
        Some("scripted")
    );
    assert_eq!(client.tools().len(), 1);
    assert_eq!(client.tools()[0].name, "echo");
}

/// A server-initiated request carries a method and an id but no result. Without
/// a guard the reader treats it as a response and resolves whichever caller
/// happens to hold that id with a null result.
#[tokio::test]
async fn a_server_initiated_request_does_not_resolve_a_pending_call() {
    let Some(python) = python() else {
        eprintln!("skipping: no Python interpreter available");
        return;
    };

    let extra = r#"
# tools/call — answer with a ping request first, reusing the caller's id.
request = read()
send({"jsonrpc": "2.0", "id": request["id"], "method": "ping"})
send({"jsonrpc": "2.0", "id": request["id"], "result": {
    "content": [{"type": "text", "text": "real result"}], "isError": False}})
read()
"#;

    let config = scripted_server(python, &handshake_server(extra));
    let client = McpStdioClient::connect(&config)
        .await
        .expect("the handshake should succeed");

    let result = client
        .call_tool("echo", None)
        .await
        .expect("the real response should resolve the call");

    assert_eq!(
        result.content[0].text.as_deref(),
        Some("real result"),
        "a ping request must not be mistaken for the response"
    );
}

/// A request that times out must remove its pending entry. A leak here is
/// bounded by request count, but a long-lived agent session makes many.
#[tokio::test]
async fn a_timed_out_request_does_not_leak_its_pending_entry() {
    let Some(python) = python() else {
        eprintln!("skipping: no Python interpreter available");
        return;
    };

    let extra = r#"
# Swallow one tools/call without answering, then answer the next.
read()
request = read()
send({"jsonrpc": "2.0", "id": request["id"], "result": {
    "content": [{"type": "text", "text": "second call"}], "isError": False}})
read()
"#;

    let config = scripted_server(python, &handshake_server(extra));
    let client = McpStdioClient::connect(&config)
        .await
        .expect("the handshake should succeed");

    let timed_out = tokio::time::timeout(
        Duration::from_secs(5),
        client.call_tool_with_timeout("echo", None, Duration::from_millis(150)),
    )
    .await
    .expect("the call should give up on its own")
    .expect_err("no response arrives for the first call");
    assert!(matches!(timed_out, McpClientError::Timeout(_)));

    assert_eq!(
        client.pending_len().await,
        0,
        "a timed-out request must not leave an entry behind"
    );

    let result = client
        .call_tool("echo", None)
        .await
        .expect("the connection should remain usable");
    assert_eq!(result.content[0].text.as_deref(), Some("second call"));
}

#[tokio::test]
async fn every_pending_call_fails_when_the_process_exits() {
    let Some(python) = python() else {
        eprintln!("skipping: no Python interpreter available");
        return;
    };

    // Exit immediately after the handshake, without answering the tool call.
    let config = scripted_server(python, &handshake_server("raise SystemExit(0)"));
    let client = McpStdioClient::connect(&config)
        .await
        .expect("the handshake should succeed");

    let error = tokio::time::timeout(Duration::from_secs(10), client.call_tool("echo", None))
        .await
        .expect("the call must fail rather than hang")
        .expect_err("a dead process cannot answer");
    assert!(
        matches!(error, McpClientError::ProcessExited),
        "got {error:?}"
    );
}
