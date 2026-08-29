//! Tests for the MCP stdio client, driven by a scripted server process.
//!
//! The server is a short Python program so the test can control exactly which
//! JSON-RPC frames come back, including ones a well-behaved server would never
//! send. Tests that need it are skipped when no interpreter is available rather
//! than failing, so the suite still runs on a machine without Python.

use std::collections::HashMap;
use std::time::Duration;

#[cfg(windows)]
use std::process::Stdio;

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
import os, sys, json

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
    {{"name": "echo", "description": json.dumps(dict(os.environ)),
      "inputSchema": {{"type": "object"}}}}]}}}})

{extra}
"#
    )
}

fn server_environment(client: &McpStdioClient) -> HashMap<String, String> {
    let description = client.tools()[0]
        .description
        .as_deref()
        .expect("the scripted server reports its environment");
    serde_json::from_str(description).expect("the environment is valid JSON")
}

/// Runs this one test in a child test process whose environment contains a
/// name no normal test or MCP config uses. Mutating the current process's
/// environment is unsafe once the test runner has threads; a child process is
/// the deterministic way to prove inheritance without a process-global race.
fn rerun_with_host_only_variable(test_name: &str) -> Option<std::process::ExitStatus> {
    const MARKER: &str = "MENTRA_MCP_ENV_TEST_CHILD";
    const HOST_ONLY: &str = "MENTRA_MCP_HOST_ONLY";

    if std::env::var_os(MARKER).is_some() {
        return None;
    }

    Some(
        std::process::Command::new(std::env::current_exe().expect("current test executable"))
            .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
            .env(MARKER, "1")
            .env(HOST_ONLY, "ambient-secret")
            .status()
            .expect("rerun the test with a host-only variable"),
    )
}

#[tokio::test]
async fn stdio_server_receives_only_the_baseline_and_explicit_environment() {
    const TEST_NAME: &str =
        "mcp::client::tests::stdio_server_receives_only_the_baseline_and_explicit_environment";
    const HOST_ONLY: &str = "MENTRA_MCP_HOST_ONLY";

    if let Some(status) = rerun_with_host_only_variable(TEST_NAME) {
        assert!(
            status.success(),
            "the isolated test process failed: {status}"
        );
        return;
    }

    let Some(python) = python() else {
        eprintln!("skipping: no Python interpreter available");
        return;
    };

    assert_eq!(
        std::env::var(HOST_ONLY).as_deref(),
        Ok("ambient-secret"),
        "the host-only variable must exist in the client process"
    );

    let source = handshake_server("read()");
    let config = scripted_server(python, &source);
    let client = McpStdioClient::connect(&config)
        .await
        .expect("the handshake should succeed with the baseline PATH");
    let environment = server_environment(&client);

    assert!(
        environment.contains_key("PATH"),
        "the baseline must keep a bare interpreter command runnable: {environment:?}"
    );
    assert!(
        !environment.contains_key(HOST_ONLY),
        "an ambient host variable reached the server: {environment:?}"
    );
    drop(client);

    let mut explicit = scripted_server(python, &source);
    explicit
        .env
        .insert(HOST_ONLY.to_string(), "declared-value".to_string());
    let client = McpStdioClient::connect(&explicit)
        .await
        .expect("an explicitly configured variable should be accepted");
    assert_eq!(
        server_environment(&client)
            .get(HOST_ONLY)
            .map(String::as_str),
        Some("declared-value")
    );
}

#[tokio::test]
async fn dropping_a_stdio_client_kills_its_descendants() {
    let Some(python) = python() else {
        eprintln!("skipping: no Python interpreter available");
        return;
    };

    let source = r#"
import json, subprocess, sys, time

descendant = subprocess.Popen(
    [sys.executable, "-c", "import time; time.sleep(60)"],
    stdin=subprocess.DEVNULL,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL)

def send(payload):
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()

def read():
    line = sys.stdin.readline()
    if not line:
        raise SystemExit(0)
    return json.loads(line)

request = read()
send({"jsonrpc": "2.0", "id": request["id"], "result": {
    "protocolVersion": "2024-11-05", "capabilities": {},
    "serverInfo": {"name": "descendant", "version": "1"}}})
read()
request = read()
send({"jsonrpc": "2.0", "id": request["id"], "result": {"tools": [{
    "name": "pid", "description": str(descendant.pid),
    "inputSchema": {"type": "object"}}]}})
read()
"#;

    let config = scripted_server(python, source);
    let client = McpStdioClient::connect(&config)
        .await
        .expect("the handshake should succeed");
    let pid: u32 = client.tools()[0]
        .description
        .as_deref()
        .expect("the server reports its descendant")
        .parse()
        .expect("the descendant pid is numeric");

    drop(client);

    let dead = wait_until_process_is_dead(pid).await;
    if !dead {
        // Keep the RED run from leaving the fixture behind on old production.
        kill_process(pid);
    }
    assert!(dead, "MCP server descendant {pid} survived client drop");
}

#[cfg(unix)]
async fn wait_until_process_is_dead(pid: u32) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if unsafe { libc::kill(pid as i32, 0) } == -1 {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(windows)]
async fn wait_until_process_is_dead(pid: u32) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let running = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()));
        if !running {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(unix)]
fn kill_process(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

#[cfg(windows)]
fn kill_process(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[tokio::test]
async fn an_oversized_stdio_response_is_rejected() {
    let Some(python) = python() else {
        eprintln!("skipping: no Python interpreter available");
        return;
    };

    let source = r#"
import json, sys

def send(payload):
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()

def read():
    line = sys.stdin.readline()
    if not line:
        raise SystemExit(0)
    return json.loads(line)

request = read()
send({"jsonrpc": "2.0", "id": request["id"], "result": {
    "protocolVersion": "2024-11-05", "capabilities": {},
    "serverInfo": {"name": "oversized", "version": "1"},
    "padding": "x" * (8 * 1024 * 1024)}})
read()
request = read()
send({"jsonrpc": "2.0", "id": request["id"], "result": {"tools": []}})
read()
"#;

    let config = scripted_server(python, source);
    let error = match McpStdioClient::connect(&config).await {
        Ok(_) => panic!("one stdio response must have a finite memory bound"),
        Err(error) => error,
    };
    assert!(matches!(error, McpClientError::ProcessExited), "{error:?}");
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
