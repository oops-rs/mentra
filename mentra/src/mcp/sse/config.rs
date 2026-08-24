//! Configuration for the legacy MCP HTTP+SSE transport.

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;
use url::Url;

use super::endpoint::{EndpointError, is_loopback, validate_configured_url};
// Re-exported so `mcp::sse::config::SecretString` keeps resolving now that the
// type is shared with the Streamable HTTP transport.
pub use crate::mcp::secret::SecretString;

/// Default timeout for opening the SSE stream and reading its response head.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default timeout for the MCP `initialize` handshake, matching the stdio client.
pub const DEFAULT_INITIALIZE_TIMEOUT: Duration = Duration::from_secs(10);
/// Default timeout for `tools/list`, matching the stdio client.
pub const DEFAULT_LIST_TOOLS_TIMEOUT: Duration = Duration::from_secs(30);
/// Default timeout for `tools/call`, matching the stdio client.
pub const DEFAULT_CALL_TOOL_TIMEOUT: Duration = Duration::from_secs(120);
/// Default idle timeout between stream reads.
///
/// Servers built on `sse-starlette` — which covers most Python MCP servers —
/// emit a comment heartbeat every 15 seconds, so five minutes of silence means
/// the stream is dead rather than quiet.
pub const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// Default cap on the bytes buffered for a single SSE event.
///
/// The largest legitimate event is a `tools/call` result carrying base64
/// content; 4 MiB of base64 is roughly 3 MB of binary, which is generous for a
/// tool result while bounding worst-case memory per connection.
pub const DEFAULT_MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;
/// Default cap on the bytes buffered for the `endpoint` event specifically.
///
/// The endpoint event is processed before any request correlation exists, so it
/// is the earliest attacker-reachable allocation in the connection lifecycle.
/// Its payload is a single URL, and common proxy header limits sit near 2 KiB.
pub const DEFAULT_MAX_ENDPOINT_BYTES: usize = 8 * 1024;
/// Default cap on how many `tools/list` pages are followed.
///
/// Cursors are opaque, so a server repeating one cannot be detected by value;
/// only a page bound stops the walk. A server needing more pages than this to
/// describe its tools is malfunctioning.
pub const DEFAULT_MAX_TOOL_PAGES: usize = 1_000;

/// Timeouts and size limits for one SSE connection.
///
/// These are separated from the operator-facing fields of
/// [`McpSseServerConfig`] because they are tuning knobs for the host rather
/// than something an operator writes in a configuration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSseLimits {
    /// Bound on opening the stream and reading its response head.
    pub connect_timeout: Duration,
    /// Bound on the `initialize` handshake.
    pub initialize_timeout: Duration,
    /// Bound on each `tools/list` page.
    pub list_tools_timeout: Duration,
    /// Bound on each `tools/call`.
    pub call_tool_timeout: Duration,
    /// Bound on silence between reads on the SSE stream.
    pub stream_idle_timeout: Duration,
    /// Bound on the bytes buffered for a single SSE event.
    pub max_event_bytes: usize,
    /// Bound on the bytes buffered for the `endpoint` event.
    pub max_endpoint_bytes: usize,
    /// Bound on how many `tools/list` pages are followed.
    pub max_tool_pages: usize,
}

impl Default for McpSseLimits {
    fn default() -> Self {
        Self {
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            initialize_timeout: DEFAULT_INITIALIZE_TIMEOUT,
            list_tools_timeout: DEFAULT_LIST_TOOLS_TIMEOUT,
            call_tool_timeout: DEFAULT_CALL_TOOL_TIMEOUT,
            stream_idle_timeout: DEFAULT_STREAM_IDLE_TIMEOUT,
            max_event_bytes: DEFAULT_MAX_EVENT_BYTES,
            max_endpoint_bytes: DEFAULT_MAX_ENDPOINT_BYTES,
            max_tool_pages: DEFAULT_MAX_TOOL_PAGES,
        }
    }
}

/// Configuration for an MCP server reachable over the legacy HTTP+SSE
/// transport.
///
/// This is the SSE counterpart to [`McpServerConfig`](crate::mcp::McpServerConfig),
/// which remains the stdio configuration type.
///
/// # Example
///
/// ```rust
/// use mentra::mcp::McpSseServerConfig;
///
/// let config = McpSseServerConfig::new("observability", "https://mcp.example.com/sse")
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
pub struct McpSseServerConfig {
    /// Display name for the server, used to namespace its bridged tools.
    pub name: String,
    /// The operator-configured SSE stream URL, opened with a long-lived `GET`.
    pub url: String,
    /// Headers sent on both the SSE `GET` and every JSON-RPC `POST`.
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
    pub limits: McpSseLimits,
}

/// Errors from validating an [`McpSseServerConfig`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum McpSseConfigError {
    #[error("invalid MCP SSE stream URL: {0}")]
    Url(#[from] EndpointError),

    #[error("MCP SSE server name must not be empty")]
    EmptyName,

    #[error("invalid MCP SSE header name '{name}'")]
    InvalidHeaderName { name: String },

    /// Rendered without the value so a malformed credential never reaches a log.
    #[error("MCP SSE header '{name}' has a value that is not valid for HTTP")]
    InvalidHeaderValue { name: String },

    #[error(
        "refusing to send configured headers to '{url}' over plaintext http; \
         use https, a loopback host, or set allow_plaintext_credentials"
    )]
    PlaintextCredentials { url: String },
}

impl McpSseServerConfig {
    /// Creates a configuration with default timeouts and limits.
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
            headers: BTreeMap::new(),
            allow_plaintext_credentials: false,
            limits: McpSseLimits::default(),
        }
    }

    /// Adds a header sent on both the SSE `GET` and every JSON-RPC `POST`.
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
    pub fn with_limits(mut self, limits: McpSseLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Permits sending configured headers over plaintext `http://`.
    pub fn allowing_plaintext_credentials(mut self) -> Self {
        self.allow_plaintext_credentials = true;
        self
    }

    /// Validates the configuration and returns the parsed stream URL.
    ///
    /// Runs before any connection is opened so a bad configuration fails at the
    /// boundary rather than mid-handshake.
    /// Checks the URL, header names, and credential handling without
    /// connecting, so a host can reject a bad configuration at its own
    /// boundary rather than discovering it mid-build.
    pub fn validate(&self) -> Result<Url, McpSseConfigError> {
        if self.name.trim().is_empty() {
            return Err(McpSseConfigError::EmptyName);
        }

        let url = validate_configured_url(&self.url)?;

        for (name, value) in &self.headers {
            if reqwest::header::HeaderName::try_from(name.as_str()).is_err() {
                return Err(McpSseConfigError::InvalidHeaderName {
                    name: name.to_string(),
                });
            }
            if reqwest::header::HeaderValue::try_from(value.expose_secret()).is_err() {
                return Err(McpSseConfigError::InvalidHeaderValue {
                    name: name.to_string(),
                });
            }
        }

        if !self.headers.is_empty()
            && url.scheme() == "http"
            && !self.allow_plaintext_credentials
            && !is_loopback(&url)
        {
            return Err(McpSseConfigError::PlaintextCredentials {
                url: self.url.clone(),
            });
        }

        Ok(url)
    }
}
