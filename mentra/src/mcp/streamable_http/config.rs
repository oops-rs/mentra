//! Configuration for the MCP Streamable HTTP transport.

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;
use url::Url;

use crate::mcp::secret::SecretString;
use crate::mcp::sse::endpoint::{EndpointError, is_loopback, validate_configured_url};

/// Default timeout for establishing the TCP and TLS connection.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default timeout for the MCP `initialize` handshake, matching the stdio client.
pub const DEFAULT_INITIALIZE_TIMEOUT: Duration = Duration::from_secs(10);
/// Default timeout for `tools/list`, matching the stdio client.
pub const DEFAULT_LIST_TOOLS_TIMEOUT: Duration = Duration::from_secs(30);
/// Default timeout for `tools/call`, matching the stdio client.
pub const DEFAULT_CALL_TOOL_TIMEOUT: Duration = Duration::from_secs(120);
/// Default idle timeout between reads on a reply that arrives as an SSE stream.
///
/// A server may hold such a stream open while it works, sending progress
/// notifications, so silence rather than duration is what distinguishes a dead
/// stream from a slow one. The per-request deadline still bounds the whole
/// operation, so this only decides how long a stalled body is tolerated within
/// it.
pub const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Default cap on the bytes buffered for a single SSE event.
///
/// The largest legitimate event is a `tools/call` result carrying base64
/// content; 4 MiB of base64 is roughly 3 MB of binary, which is generous for a
/// tool result while bounding worst-case memory per connection.
pub const DEFAULT_MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;
/// Default cap on the bytes read for one reply.
///
/// This is the only bound on the immediate `application/json` path, where the
/// whole reply is a single body, and it additionally bounds an SSE reply whose
/// individually-legal events never stop arriving.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Default cap on how many `tools/list` pages are followed.
///
/// Cursors are opaque, so a server repeating one cannot be detected by value;
/// only a page bound stops the walk. A server needing more pages than this to
/// describe its tools is malfunctioning.
pub const DEFAULT_MAX_TOOL_PAGES: usize = 1_000;

/// Default cap on how many tools a server may advertise in total.
///
/// The page cap bounds how many round trips a walk makes and
/// `max_response_bytes` bounds each page, but nothing bounded the list they
/// accumulate into: a server willing to fill every page could push gigabytes
/// through the walk and be stopped by the allocator rather than by a limit.
/// Well past what any real server exposes.
pub const DEFAULT_MAX_TOOLS: usize = 4_096;

/// Headers this transport sets per request, which a configuration may not
/// also supply.
///
/// Both are protocol state the client owns: the session id is assigned by the
/// server during `initialize`, and the protocol version is the one negotiated
/// there. A configured value would either be overwritten or silently duplicated
/// depending on insertion order, so it is refused at the boundary instead.
const RESERVED_HEADERS: [&str; 2] = ["mcp-session-id", "mcp-protocol-version"];

/// Timeouts and size limits for one Streamable HTTP connection.
///
/// These are separated from the operator-facing fields of
/// [`McpStreamableHttpServerConfig`] because they are tuning knobs for the host
/// rather than something an operator writes in a configuration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpStreamableHttpLimits {
    /// Bound on establishing the connection.
    pub connect_timeout: Duration,
    /// Bound on the `initialize` handshake.
    pub initialize_timeout: Duration,
    /// Bound on each `tools/list` page.
    pub list_tools_timeout: Duration,
    /// Bound on each `tools/call`.
    pub call_tool_timeout: Duration,
    /// Bound on silence between reads of a reply that arrives as an SSE stream.
    pub stream_idle_timeout: Duration,
    /// Bound on the bytes buffered for a single SSE event.
    pub max_event_bytes: usize,
    /// Bound on the bytes read for one reply.
    pub max_response_bytes: usize,
    /// Bound on how many `tools/list` pages are followed.
    pub max_tool_pages: usize,
    /// Bound on how many tools a server may advertise in total.
    pub max_tools: usize,
}

impl Default for McpStreamableHttpLimits {
    fn default() -> Self {
        Self {
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            initialize_timeout: DEFAULT_INITIALIZE_TIMEOUT,
            list_tools_timeout: DEFAULT_LIST_TOOLS_TIMEOUT,
            call_tool_timeout: DEFAULT_CALL_TOOL_TIMEOUT,
            stream_idle_timeout: DEFAULT_STREAM_IDLE_TIMEOUT,
            max_event_bytes: DEFAULT_MAX_EVENT_BYTES,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_tool_pages: DEFAULT_MAX_TOOL_PAGES,
            max_tools: DEFAULT_MAX_TOOLS,
        }
    }
}

/// Configuration for an MCP server reachable over the Streamable HTTP
/// transport.
///
/// This is the transport from protocol revision 2025-03-26 and later, where one
/// URL serves every message. It is the counterpart to
/// [`McpServerConfig`](crate::mcp::McpServerConfig) for stdio and
/// [`McpSseServerConfig`](crate::mcp::McpSseServerConfig) for the legacy
/// HTTP+SSE transport.
///
/// # Example
///
/// ```rust
/// use mentra::mcp::McpStreamableHttpServerConfig;
///
/// let config = McpStreamableHttpServerConfig::new("observability", "https://mcp.example.com/mcp")
///     .with_header("authorization", "Bearer <token>");
/// ```
///
/// # Security
///
/// Header values are stored as [`SecretString`] and never appear in `Debug`
/// output, error messages, or logs. Configuring a header on a plaintext `http://`
/// URL is rejected unless the host is loopback, because the credential would
/// otherwise cross the network in the clear; see
/// [`allow_plaintext_credentials`](Self::allow_plaintext_credentials) to
/// override that deliberately.
#[derive(Debug, Clone, Deserialize)]
pub struct McpStreamableHttpServerConfig {
    /// Display name for the server, used to namespace its bridged tools.
    pub name: String,
    /// The operator-configured MCP endpoint, which serves every request.
    pub url: String,
    /// Headers sent on every request to the endpoint.
    #[serde(default)]
    pub headers: BTreeMap<String, SecretString>,
    /// Permits sending configured headers over plaintext `http://` to a
    /// non-loopback host.
    ///
    /// This exists so the refusal is overridable but never accidental.
    #[serde(default)]
    pub allow_plaintext_credentials: bool,
    /// Timeouts and size limits, defaulted rather than deserialized.
    #[serde(skip)]
    pub limits: McpStreamableHttpLimits,
}

/// Errors from validating an [`McpStreamableHttpServerConfig`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum McpStreamableHttpConfigError {
    #[error("invalid MCP endpoint URL: {0}")]
    Url(#[from] EndpointError),

    #[error("MCP server name must not be empty")]
    EmptyName,

    #[error("invalid MCP header name '{name}'")]
    InvalidHeaderName { name: String },

    /// Rendered without the value so a malformed credential never reaches a log.
    #[error("MCP header '{name}' has a value that is not valid for HTTP")]
    InvalidHeaderValue { name: String },

    #[error("MCP header '{name}' is set by the transport and must not be configured")]
    ReservedHeader { name: String },

    #[error(
        "refusing to send configured headers to '{url}' over plaintext http; \
         use https, a loopback host, or set allow_plaintext_credentials"
    )]
    PlaintextCredentials { url: String },
}

impl McpStreamableHttpServerConfig {
    /// Creates a configuration with default timeouts and limits.
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
            headers: BTreeMap::new(),
            allow_plaintext_credentials: false,
            limits: McpStreamableHttpLimits::default(),
        }
    }

    /// Adds a header sent on every request to the endpoint.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<SecretString>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// Adds a bearer `Authorization` header.
    pub fn with_bearer_token(self, token: impl Into<String>) -> Self {
        self.with_header(
            "authorization",
            SecretString::new(format!("Bearer {}", token.into())),
        )
    }

    /// Replaces the timeouts and size limits.
    pub fn with_limits(mut self, limits: McpStreamableHttpLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Permits sending configured headers over plaintext `http://`.
    pub fn allowing_plaintext_credentials(mut self) -> Self {
        self.allow_plaintext_credentials = true;
        self
    }

    /// Validates the configuration and returns the parsed endpoint URL.
    ///
    /// Checks the URL, header names, and credential handling without
    /// connecting, so a host can reject a bad configuration at its own boundary
    /// rather than discovering it mid-handshake.
    pub fn validate(&self) -> Result<Url, McpStreamableHttpConfigError> {
        if self.name.trim().is_empty() {
            return Err(McpStreamableHttpConfigError::EmptyName);
        }

        let url = validate_configured_url(&self.url)?;

        for (name, value) in &self.headers {
            if reqwest::header::HeaderName::try_from(name.as_str()).is_err() {
                return Err(McpStreamableHttpConfigError::InvalidHeaderName {
                    name: name.to_string(),
                });
            }
            if RESERVED_HEADERS
                .iter()
                .any(|reserved| name.eq_ignore_ascii_case(reserved))
            {
                return Err(McpStreamableHttpConfigError::ReservedHeader {
                    name: name.to_string(),
                });
            }
            if reqwest::header::HeaderValue::try_from(value.expose_secret()).is_err() {
                return Err(McpStreamableHttpConfigError::InvalidHeaderValue {
                    name: name.to_string(),
                });
            }
        }

        if !self.headers.is_empty()
            && url.scheme() == "http"
            && !self.allow_plaintext_credentials
            && !is_loopback(&url)
        {
            return Err(McpStreamableHttpConfigError::PlaintextCredentials {
                url: self.url.clone(),
            });
        }

        Ok(url)
    }
}
