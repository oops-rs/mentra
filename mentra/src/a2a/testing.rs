//! A local A2A agent for tests.
//!
//! Like the MCP HTTP fixtures, this is a real [`TcpListener`] rather than a
//! mock, so that what is asserted is what a server receives: the
//! `A2A-Version` header the reference server refuses a request without, the
//! configured credentials, and the request bodies and paths themselves.
//!
//! It serves both bindings from one script: JSON-RPC at `/rpc`, HTTP+JSON
//! under `/rest`, and the card on every other `GET`. Which one a client
//! speaks is up to the card, as it is against a real agent.
//!
//! [`TcpListener`]: std::net::TcpListener

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use a2a_lf::{ERROR_INFO_TYPE, PROTOCOL_DOMAIN, error_code, error_reason};
use serde_json::{Value, json};

use crate::a2a::A2aBinding;
use crate::mcp::testing::{CapturedRequest, read_request};

/// The prefix every HTTP+JSON path is served under.
const REST_PREFIX: &str = "/rest";

/// What the fixture answers one operation with.
#[derive(Debug, Clone)]
pub(crate) enum RpcAnswer {
    /// A result: the JSON-RPC `result`, or the HTTP+JSON body.
    Result(Value),
    /// An A2A error with this JSON-RPC code — over HTTP+JSON, its reason —
    /// and a message that must never surface.
    Error(i32),
    /// This HTTP status and raw body, bypassing either binding's envelope.
    Http(u16, String),
}

/// A scripted agent: a card, and an answer per operation.
#[derive(Debug, Clone)]
pub(crate) struct Script {
    /// The card served on every `GET` outside `/rest`; `None` serves
    /// [`standard_card`] for [`binding`](Self::binding).
    pub(crate) card: Option<Value>,
    /// The binding the default card offers first.
    pub(crate) binding: A2aBinding,
    /// The tenant the default card's offered interface names.
    pub(crate) tenant: Option<&'static str>,
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
            binding: A2aBinding::JsonRpc,
            tenant: None,
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

impl Script {
    /// The default script, offering `binding` first.
    pub(crate) fn over(binding: A2aBinding) -> Self {
        Self {
            binding,
            ..Self::default()
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

    /// The operations received, in order, by their JSON-RPC method names
    /// whichever binding carried them.
    pub(crate) fn operations(&self) -> Vec<String> {
        self.requests().iter().filter_map(operation_of).collect()
    }

    /// The request message of every `method` received, in order: the
    /// JSON-RPC `params`, or the HTTP+JSON body (a `GET`'s query, as
    /// strings).
    pub(crate) fn params_of(&self, method: &str) -> Vec<Value> {
        self.requests()
            .iter()
            .filter(|request| operation_of(request).as_deref() == Some(method))
            .filter_map(request_message)
            .collect()
    }
}

struct State {
    base: String,
    script: Script,
    gets: AtomicUsize,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
}

/// A card offering `binding` at `{base}/rpc` or `{base}/rest`, after a gRPC
/// interface the client must pass over, and before the other binding, so the
/// card's order is what chooses.
pub(crate) fn standard_card(base: &str, binding: A2aBinding) -> Value {
    let jsonrpc = json!({"url": format!("{base}/rpc"), "protocolBinding": "JSONRPC", "protocolVersion": "1.0"});
    let rest = json!({"url": format!("{base}{REST_PREFIX}"), "protocolBinding": "HTTP+JSON", "protocolVersion": "1.0"});
    let (first, second) = match binding {
        A2aBinding::HttpJson => (rest, jsonrpc),
        _ => (jsonrpc, rest),
    };
    json!({
        "name": "Nous",
        "description": "Investigates questions about the codebase.",
        "version": "1.0.0",
        "supportedInterfaces": [
            {"url": format!("{base}/grpc"), "protocolBinding": "GRPC", "protocolVersion": "1.0"},
            first,
            second,
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

/// The card a script serves when it names none.
fn default_card(base: &str, script: &Script) -> Value {
    let mut card = standard_card(base, script.binding);
    if let Some(tenant) = script.tenant {
        card["supportedInterfaces"][1]["tenant"] = json!(tenant);
    }
    card
}

/// A task in `state` with no artifacts and no status message.
pub(crate) fn task_json(id: &str, context_id: &str, state: &str) -> Value {
    json!({"id": id, "contextId": context_id, "status": {"state": state}})
}

/// The operation a request carries, by its JSON-RPC method name.
fn operation_of(request: &CapturedRequest) -> Option<String> {
    match rest_route(request) {
        Some(route) => route.operation.map(str::to_string),
        None => request.rpc_method(),
    }
}

/// The request message a request carries.
fn request_message(request: &CapturedRequest) -> Option<Value> {
    if rest_route(request).is_none() {
        return serde_json::from_str::<Value>(&request.body)
            .ok()?
            .get("params")
            .cloned();
    }
    if request.method == "POST" {
        return serde_json::from_str(&request.body).ok();
    }
    let query = request
        .target
        .split_once('?')
        .map_or("", |(_, query)| query);
    let pairs = url::form_urlencoded::parse(query.as_bytes())
        .map(|(key, value)| (key.into_owned(), Value::String(value.into_owned())));
    Some(Value::Object(pairs.collect()))
}

/// An HTTP+JSON request, by the operation its verb and path name.
struct RestRoute {
    operation: Option<&'static str>,
}

/// Reads an HTTP+JSON path — `/rest`, an optional tenant, then the
/// operation's own segments — into the operation it names.
fn rest_route(request: &CapturedRequest) -> Option<RestRoute> {
    let path = request.target.split('?').next()?;
    let rest = path.strip_prefix(REST_PREFIX)?;
    let segments: Vec<&str> = rest.split('/').filter(|part| !part.is_empty()).collect();
    // A tenant precedes the first segment an operation's path starts with.
    let start = segments.iter().position(|segment| {
        segment.starts_with("message:") || *segment == "tasks" || *segment == "extendedAgentCard"
    });
    let operation = start.and_then(|start| {
        let verb = request.method.as_str();
        Some(match (verb, &segments[start..]) {
            ("POST", ["message:send"]) => "SendMessage",
            ("POST", ["message:stream"]) => "SendStreamingMessage",
            ("GET", ["tasks"]) => "ListTasks",
            ("GET", ["extendedAgentCard"]) => "GetExtendedAgentCard",
            ("POST", ["tasks", id]) if id.ends_with(":cancel") => "CancelTask",
            ("GET", ["tasks", id]) if id.ends_with(":subscribe") => "SubscribeToTask",
            ("GET", ["tasks", _]) => "GetTask",
            ("POST", ["tasks", _, "pushNotificationConfigs"]) => "CreateTaskPushNotificationConfig",
            ("GET", ["tasks", _, "pushNotificationConfigs"]) => "ListTaskPushNotificationConfigs",
            ("GET", ["tasks", _, "pushNotificationConfigs", _]) => "GetTaskPushNotificationConfig",
            ("DELETE", ["tasks", _, "pushNotificationConfigs", _]) => {
                "DeleteTaskPushNotificationConfig"
            }
            _ => return None,
        })
    });
    Some(RestRoute { operation })
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
    let route = rest_route(request);
    if request.method == "GET" && route.is_none() {
        let card = state
            .script
            .card
            .clone()
            .unwrap_or_else(|| default_card(&state.base, &state.script));
        return (200, card.to_string());
    }

    let answer = match operation_of(request).as_deref() {
        Some("SendMessage") => state.script.send.clone(),
        Some("GetTask") => {
            let index = state.gets.fetch_add(1, Ordering::SeqCst);
            let gets = &state.script.get;
            gets[index.min(gets.len() - 1)].clone()
        }
        Some("CancelTask") => state.script.cancel.clone(),
        _ => RpcAnswer::Error(error_code::METHOD_NOT_FOUND),
    };

    match route {
        Some(_) => rest_answer(answer),
        None => jsonrpc_answer(request, answer),
    }
}

/// An answer in JSON-RPC's envelope.
fn jsonrpc_answer(request: &CapturedRequest, answer: RpcAnswer) -> (u16, String) {
    let id = serde_json::from_str::<Value>(&request.body)
        .ok()
        .and_then(|body| body.get("id").cloned())
        .unwrap_or(Value::Null);
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

/// An answer in HTTP+JSON's terms: a result is the body, an error a
/// `google.rpc.Status` with the HTTP status the specification maps it to.
fn rest_answer(answer: RpcAnswer) -> (u16, String) {
    match answer {
        RpcAnswer::Result(result) => (200, result.to_string()),
        RpcAnswer::Error(code) => (rest_status(code), rest_error(code).to_string()),
        RpcAnswer::Http(status, body) => (status, body),
    }
}

/// The `{"error": google.rpc.Status}` body naming an A2A error.
pub(crate) fn rest_error(code: i32) -> Value {
    json!({"error": {
        "code": rest_status(code),
        "status": "FAILED_PRECONDITION",
        "message": "\u{1b}[31mserver text that must not surface",
        "details": [{
            "@type": ERROR_INFO_TYPE,
            "reason": error_reason(code),
            "domain": PROTOCOL_DOMAIN,
        }],
    }})
}

fn rest_status(code: i32) -> u16 {
    match code {
        error_code::TASK_NOT_FOUND => 404,
        error_code::METHOD_NOT_FOUND => 404,
        error_code::INVALID_AGENT_RESPONSE | error_code::INTERNAL_ERROR => 500,
        _ => 400,
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
