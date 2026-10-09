//! A local A2A agent for tests.
//!
//! Like the MCP HTTP fixtures, this is a real [`TcpListener`] rather than a
//! mock, so that what is asserted is what a server receives: the
//! `A2A-Version` header the reference server refuses a request without, the
//! configured credentials, and the JSON-RPC bodies themselves.
//!
//! [`TcpListener`]: std::net::TcpListener

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{Value, json};

use crate::mcp::testing::{CapturedRequest, read_request};

/// What the fixture answers one JSON-RPC method with.
#[derive(Debug, Clone)]
pub(crate) enum RpcAnswer {
    /// A JSON-RPC result.
    Result(Value),
    /// A JSON-RPC error with this code and a message that must never surface.
    Error(i32),
    /// This HTTP status and raw body, bypassing the JSON-RPC envelope.
    Http(u16, String),
}

/// A scripted agent: a card, and an answer per method.
#[derive(Debug, Clone)]
pub(crate) struct Script {
    /// The card served on every `GET`; `None` serves [`standard_card`].
    pub(crate) card: Option<Value>,
    /// The answer to `SendMessage`.
    pub(crate) send: RpcAnswer,
    /// Successive answers to `GetTask`; the last one repeats.
    pub(crate) get: Vec<RpcAnswer>,
    /// The answer to `CancelTask`.
    pub(crate) cancel: RpcAnswer,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            card: None,
            send: RpcAnswer::Result(json!({"task": task_json("t1", "c1", "TASK_STATE_COMPLETED")})),
            get: vec![RpcAnswer::Result(task_json(
                "t1",
                "c1",
                "TASK_STATE_COMPLETED",
            ))],
            cancel: RpcAnswer::Result(task_json("t1", "c1", "TASK_STATE_CANCELED")),
        }
    }
}

/// A running fixture agent.
pub(crate) struct A2aFixture {
    base: String,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
}

impl A2aFixture {
    /// Starts an agent that follows `script`.
    pub(crate) fn start(script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture listener");
        let port = listener.local_addr().expect("fixture address").port();
        let base = format!("http://127.0.0.1:{port}");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = Arc::new(State {
            base: base.clone(),
            script,
            gets: AtomicUsize::new(0),
            requests: Arc::clone(&requests),
        });

        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let state = Arc::clone(&state);
                // One thread per connection: a client may hold one keep-alive
                // connection idle while it opens another, and a serial accept
                // loop would block reading the idle one.
                thread::spawn(move || serve(stream, &state));
            }
        });

        Self { base, requests }
    }

    /// The agent's base URL, from which its well-known card is served.
    pub(crate) fn base(&self) -> &str {
        &self.base
    }

    /// Every request received, in order.
    pub(crate) fn requests(&self) -> Vec<CapturedRequest> {
        self.requests.lock().expect("fixture request log").clone()
    }

    /// The JSON-RPC methods received, in order.
    pub(crate) fn rpc_methods(&self) -> Vec<String> {
        self.requests()
            .iter()
            .filter_map(CapturedRequest::rpc_method)
            .collect()
    }

    /// The `params` of every request for `method`, in order.
    pub(crate) fn params_of(&self, method: &str) -> Vec<Value> {
        self.requests()
            .iter()
            .filter(|request| request.rpc_method().as_deref() == Some(method))
            .filter_map(|request| {
                serde_json::from_str::<Value>(&request.body)
                    .ok()?
                    .get("params")
                    .cloned()
            })
            .collect()
    }
}

struct State {
    base: String,
    script: Script,
    gets: AtomicUsize,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
}

/// A card offering one JSON-RPC 1.0 interface at `{base}/rpc`.
pub(crate) fn standard_card(base: &str) -> Value {
    json!({
        "name": "Nous",
        "description": "Investigates questions about the codebase.",
        "version": "1.0.0",
        "supportedInterfaces": [
            {"url": format!("{base}/grpc"), "protocolBinding": "GRPC", "protocolVersion": "1.0"},
            {"url": format!("{base}/rpc"), "protocolBinding": "JSONRPC", "protocolVersion": "1.0"},
        ],
        "capabilities": {"streaming": false},
        "defaultInputModes": ["text/plain"],
        "defaultOutputModes": ["text/plain"],
        "skills": [{
            "id": "investigate",
            "name": "Investigate",
            "description": "Answers a question by reading the code.",
            "tags": ["code"],
        }],
    })
}

/// A task in `state` with no artifacts and no status message.
pub(crate) fn task_json(id: &str, context_id: &str, state: &str) -> Value {
    json!({"id": id, "contextId": context_id, "status": {"state": state}})
}

fn serve(mut stream: TcpStream, state: &State) {
    while let Some(request) = read_request(&mut stream) {
        state
            .requests
            .lock()
            .expect("fixture request log")
            .push(request.clone());
        let (status, body) = answer(state, &request);
        if !write_response(&mut stream, status, &body) {
            break;
        }
    }
}

fn answer(state: &State, request: &CapturedRequest) -> (u16, String) {
    if request.method == "GET" {
        let card = state
            .script
            .card
            .clone()
            .unwrap_or_else(|| standard_card(&state.base));
        return (200, card.to_string());
    }

    let id = serde_json::from_str::<Value>(&request.body)
        .ok()
        .and_then(|body| body.get("id").cloned())
        .unwrap_or(Value::Null);
    let answer = match request.rpc_method().as_deref() {
        Some("SendMessage") => state.script.send.clone(),
        Some("GetTask") => {
            let index = state.gets.fetch_add(1, Ordering::SeqCst);
            let gets = &state.script.get;
            gets[index.min(gets.len() - 1)].clone()
        }
        Some("CancelTask") => state.script.cancel.clone(),
        _ => RpcAnswer::Error(-32601),
    };

    match answer {
        RpcAnswer::Result(result) => (
            200,
            json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
        ),
        RpcAnswer::Error(code) => (
            200,
            json!({"jsonrpc": "2.0", "id": id, "error": {
                "code": code,
                "message": "\u{1b}[31mserver text that must not surface",
            }})
            .to_string(),
        ),
        RpcAnswer::Http(status, body) => (status, body),
    }
}

fn write_response(stream: &mut TcpStream, status: u16, body: &str) -> bool {
    let head = format!(
        "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).is_ok()
        && stream.write_all(body.as_bytes()).is_ok()
        && stream.flush().is_ok()
}
