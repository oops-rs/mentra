//! HTTP request reading shared by the MCP transport fixtures.
//!
//! Both HTTP transports are tested against a raw [`TcpListener`] fixture rather
//! than a mock, and both need the same thing from the socket: read one complete
//! request, honoring keep-alive, and capture what the client actually sent. The
//! reader lives here so the two fixtures cannot drift in what they consider a
//! request.
//!
//! [`TcpListener`]: std::net::TcpListener

use std::collections::HashMap;
use std::io::Read;
use std::net::TcpStream;

/// A request captured by a fixture.
#[derive(Debug, Clone)]
pub(crate) struct CapturedRequest {
    pub(crate) method: String,
    pub(crate) target: String,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) body: String,
}

impl CapturedRequest {
    /// Returns the JSON-RPC method of the captured body, if it has one.
    pub(crate) fn rpc_method(&self) -> Option<String> {
        serde_json::from_str::<serde_json::Value>(&self.body)
            .ok()?
            .get("method")?
            .as_str()
            .map(str::to_string)
    }

    /// Returns the JSON-RPC id of the captured body, if it has one.
    pub(crate) fn rpc_id(&self) -> Option<u64> {
        serde_json::from_str::<serde_json::Value>(&self.body)
            .ok()?
            .get("id")?
            .as_u64()
    }

    /// Returns a header value by its lowercase name.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

/// Reads one HTTP request, honoring keep-alive by returning `None` at EOF.
pub(crate) fn read_request(stream: &mut TcpStream) -> Option<CapturedRequest> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    let mut header_end = None;
    let mut content_length = 0_usize;

    loop {
        let read = match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(read) => read,
        };
        buffer.extend_from_slice(&chunk[..read]);

        if header_end.is_none() {
            let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
            let end = index + 4;
            header_end = Some(end);
            content_length = String::from_utf8_lossy(&buffer[..end])
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap_or_default())
                })
                .unwrap_or_default();
        }

        if header_end.is_some_and(|end| buffer.len() >= end + content_length) {
            break;
        }
    }

    let end = header_end?;
    let head = String::from_utf8_lossy(&buffer[..end]).to_string();
    let body = String::from_utf8_lossy(&buffer[end..end + content_length]).to_string();

    let mut lines = head.lines();
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_string();
    let target = request_line.next()?.to_string();

    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();

    Some(CapturedRequest {
        method,
        target,
        headers,
        body,
    })
}
