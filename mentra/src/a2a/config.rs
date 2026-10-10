//! Configuration for one remote A2A agent.

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;
use url::Url;

use super::name::{A2aAgentNameError, validate_a2a_agent_name};
use crate::mcp::SecretString;
use crate::mcp::sse::endpoint::is_loopback;

/// The path an agent publishes its card at, relative to its origin
/// (A2A §8.2, after RFC 8615).
pub const WELL_KNOWN_AGENT_CARD_PATH: &str = "/.well-known/agent-card.json";

/// Default timeout for establishing the TCP and TLS connection.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default bound on one HTTP exchange: the card fetch, or one call,
/// from sending the request to the last byte of the reply.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Default cap on the bytes read for one reply, card or call.
///
/// A task carries every artifact it has produced on every `GetTask`, so this
/// is sized for a task with a large text result rather than for one message.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Default bound on how long one tool call waits for a task to settle.
///
/// The model's turn is blocked for this long, so it is a bound on latency
/// rather than on the task: a task still working when it passes is handed
/// back with its ids, and the next call resumes waiting without resending.
pub const DEFAULT_MAX_WAIT: Duration = Duration::from_secs(300);
/// Default interval between `GetTask` polls while a task is working.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Default bound on how long an event stream may send nothing at all.
///
/// Servers keep an idle stream open with comment lines (the reference server
/// sends one every 15 seconds), and those count as activity, so this only
/// ends a stream whose connection has silently died.
pub const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Default cap on the bytes of one event in a stream.
///
/// A stream's first event is the whole task, every artifact included, so this
/// matches the cap on a reply that carries the same task.
pub const DEFAULT_MAX_EVENT_BYTES: usize = DEFAULT_MAX_RESPONSE_BYTES;
/// Default cap on the bytes of text one tool call returns.
///
/// The runtime's own tool-result policy truncates (and can spill) far below
/// this; the cap here only stops a remote agent from handing the model a
/// string as large as [`DEFAULT_MAX_RESPONSE_BYTES`] before that policy runs.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 256 * 1024;

/// Schemes this client is willing to speak.
const ALLOWED_SCHEMES: [&str; 2] = ["http", "https"];

/// Headers this client sets on every request, which a configuration may not
/// also supply.
///
/// `A2A-Version` names the protocol this client actually speaks; a configured
/// value would either be overwritten or claim a version the code does not
/// implement, so it is refused at the boundary instead.
const RESERVED_HEADERS: [&str; 1] = ["a2a-version"];

/// Timeouts and size limits for one remote agent.
///
/// Separated from the operator-facing fields of [`A2aAgentConfig`] because
/// they are tuning knobs for the host rather than something an operator
/// writes in a configuration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A2aLimits {
    /// Bound on establishing the connection.
    pub connect_timeout: Duration,
    /// Bound on one HTTP exchange, request and whole reply.
    pub request_timeout: Duration,
    /// Bound on the bytes read for one reply.
    pub max_response_bytes: usize,
    /// Bound on how long one tool call waits for a task to settle.
    pub max_wait: Duration,
    /// Interval between `GetTask` polls while a task is working.
    pub poll_interval: Duration,
    /// Bound on the bytes of text one tool call returns.
    pub max_output_bytes: usize,
    /// Bound on how long an event stream may send nothing, keep-alive
    /// comments included.
    pub stream_idle_timeout: Duration,
    /// Bound on the bytes of one event in a stream.
    pub max_event_bytes: usize,
}

impl Default for A2aLimits {
    fn default() -> Self {
        Self {
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_wait: DEFAULT_MAX_WAIT,
            poll_interval: DEFAULT_POLL_INTERVAL,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            stream_idle_timeout: DEFAULT_STREAM_IDLE_TIMEOUT,
            max_event_bytes: DEFAULT_MAX_EVENT_BYTES,
        }
    }
}

/// Configuration for one remote agent reached over A2A.
///
/// # Which URL
///
/// `url` names the agent in one of two ways, told apart by its path alone:
///
/// - **A base URL** — path `/` (or none) and no query, such as
///   `https://nous.example.com`. The card is fetched from
///   [`WELL_KNOWN_AGENT_CARD_PATH`] on that origin, where the specification
///   says an agent publishes it.
/// - **A card URL** — anything else, such as
///   `https://example.com/agents/nous/card.json`, is fetched as given.
///
/// Well-known paths only exist at an origin's root, so a URL with a path is
/// never a base for one; there is no third case to guess at.
///
/// # Example
///
/// ```rust
/// use mentra::a2a::A2aAgentConfig;
///
/// let config = A2aAgentConfig::new("nous", "https://nous.example.com")
///     .with_bearer_token("<token>");
/// assert_eq!(
///     config.card_url().unwrap().as_str(),
///     "https://nous.example.com/.well-known/agent-card.json",
/// );
/// ```
///
/// # Security
///
/// Header values are stored as [`SecretString`] and never appear in `Debug`
/// output, error messages, or logs. They are sent on the card fetch and on
/// every call. Configuring a header on a plaintext `http://` URL is
/// rejected unless the host is loopback — the same rule, with the same
/// override, as the MCP HTTP transports — and the rule is applied again to
/// the interface URL the card names, because that URL is chosen by the
/// remote side rather than by the operator.
#[derive(Clone, Deserialize)]
pub struct A2aAgentConfig {
    /// The agent's local name, which names its tool `a2a__{name}`.
    pub name: String,
    /// The agent's base URL or card URL; see [Which URL](Self#which-url).
    pub url: String,
    /// Headers sent on every request to the agent.
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
    pub limits: A2aLimits,
}

/// Hand-written so no `{:?}` of a config prints a credential. Header values
/// are [`SecretString`]s and redact themselves; `url` is a plain `String`, and
/// its userinfo and query string are where a credential would sit, so both
/// are replaced. String surgery rather than a URL parser: `Debug` must never
/// fail, and an unparseable URL is exactly what someone will debug-print.
impl std::fmt::Debug for A2aAgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A2aAgentConfig")
            .field("name", &self.name)
            .field("url", &redacted_url(&self.url))
            .field("headers", &self.headers)
            .field(
                "allow_plaintext_credentials",
                &self.allow_plaintext_credentials,
            )
            .field("limits", &self.limits)
            .finish()
    }
}

/// `url` with its userinfo and query string replaced by `[redacted]`.
fn redacted_url(url: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, _)) => (base, "?[redacted]"),
        None => (url, ""),
    };
    let base = match base.split_once("://") {
        Some((scheme, rest)) => {
            let authority_end = rest.find('/').unwrap_or(rest.len());
            match rest[..authority_end].rfind('@') {
                Some(at) => format!("{scheme}://[redacted]@{}", &rest[at + 1..]),
                None => base.to_string(),
            }
        }
        None => base.to_string(),
    };
    format!("{base}{query}")
}

/// Errors from validating an [`A2aAgentConfig`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum A2aConfigError {
    #[error(transparent)]
    InvalidName(#[from] A2aAgentNameError),

    #[error("invalid A2A agent URL: {0}")]
    Url(A2aUrlError),

    #[error("invalid A2A header name '{name}'")]
    InvalidHeaderName { name: String },

    /// Rendered without the value so a malformed credential never reaches a log.
    #[error("A2A header '{name}' has a value that is not valid for HTTP")]
    InvalidHeaderValue { name: String },

    #[error("A2A header '{name}' is set by the client and must not be configured")]
    ReservedHeader { name: String },

    /// Names the host, never the URL: a query string is where an expanded
    /// credential sits (`?key=…`), and this message reaches logs.
    #[error(
        "refusing to send configured headers to '{host}' over plaintext http; \
         use https, a loopback host, or set allow_plaintext_credentials"
    )]
    PlaintextCredentials { host: String },

    #[error("A2A limit '{name}' must be greater than zero")]
    ZeroLimit { name: &'static str },
}

/// Why a URL is not one this client will send a request to.
///
/// No variant carries the URL, because one of the two URLs checked — the
/// interface URL — is chosen by the remote agent's card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum A2aUrlError {
    #[error("the URL could not be parsed")]
    Malformed,
    #[error("only http and https URLs are allowed")]
    UnsupportedScheme,
    #[error("the URL has no host")]
    MissingHost,
    /// A password in a URL would reach every log that prints the URL.
    #[error("the URL must not embed credentials")]
    CredentialsInUrl,
}

impl A2aAgentConfig {
    /// Creates a configuration with default timeouts and limits.
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
            headers: BTreeMap::new(),
            allow_plaintext_credentials: false,
            limits: A2aLimits::default(),
        }
    }

    /// Adds a header sent on every request to the agent.
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
    pub fn with_limits(mut self, limits: A2aLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Permits sending configured headers over plaintext `http://`.
    pub fn allowing_plaintext_credentials(mut self) -> Self {
        self.allow_plaintext_credentials = true;
        self
    }

    /// The URL the Agent Card is fetched from; see
    /// [Which URL](Self#which-url).
    pub fn card_url(&self) -> Result<Url, A2aConfigError> {
        let url = check_url(&self.url).map_err(A2aConfigError::Url)?;
        if url.path() == "/" && url.query().is_none() {
            return url
                .join(WELL_KNOWN_AGENT_CARD_PATH)
                .map_err(|_| A2aConfigError::Url(A2aUrlError::Malformed));
        }
        Ok(url)
    }

    /// Validates the configuration and returns the card URL.
    ///
    /// Checks the name, URL, header names and values, credential handling and
    /// limits without connecting, so a host can reject a bad configuration at
    /// its own boundary rather than discovering it on the first request.
    pub fn validate(&self) -> Result<Url, A2aConfigError> {
        validate_a2a_agent_name(&self.name)?;
        let url = self.card_url()?;

        for (name, value) in &self.headers {
            if reqwest::header::HeaderName::try_from(name.as_str()).is_err() {
                return Err(A2aConfigError::InvalidHeaderName { name: name.clone() });
            }
            if RESERVED_HEADERS
                .iter()
                .any(|reserved| name.eq_ignore_ascii_case(reserved))
            {
                return Err(A2aConfigError::ReservedHeader { name: name.clone() });
            }
            if reqwest::header::HeaderValue::try_from(value.expose_secret()).is_err() {
                return Err(A2aConfigError::InvalidHeaderValue { name: name.clone() });
            }
        }

        if self.refuses_plaintext_to(&url) {
            return Err(A2aConfigError::PlaintextCredentials {
                host: url.host_str().unwrap_or_default().to_string(),
            });
        }

        for (name, value) in [
            ("connect_timeout", self.limits.connect_timeout),
            ("request_timeout", self.limits.request_timeout),
            ("poll_interval", self.limits.poll_interval),
            ("stream_idle_timeout", self.limits.stream_idle_timeout),
        ] {
            if value.is_zero() {
                return Err(A2aConfigError::ZeroLimit { name });
            }
        }
        for (name, value) in [
            ("max_response_bytes", self.limits.max_response_bytes),
            ("max_output_bytes", self.limits.max_output_bytes),
            ("max_event_bytes", self.limits.max_event_bytes),
        ] {
            if value == 0 {
                return Err(A2aConfigError::ZeroLimit { name });
            }
        }

        Ok(url)
    }

    /// Whether configured headers may not be sent to `url`.
    ///
    /// The MCP HTTP transports' rule, unchanged: a credential sent over
    /// plaintext never leaves the machine when the host is loopback, so only a
    /// remote plaintext host is refused, and only when there is a header to
    /// protect and the operator has not overridden the refusal.
    pub(crate) fn refuses_plaintext_to(&self, url: &Url) -> bool {
        !self.headers.is_empty()
            && url.scheme() == "http"
            && !self.allow_plaintext_credentials
            && !is_loopback(url)
    }
}

/// Parses a URL and checks it is one this client will send a request to.
pub(crate) fn check_url(raw: &str) -> Result<Url, A2aUrlError> {
    let url = Url::parse(raw.trim()).map_err(|_| A2aUrlError::Malformed)?;
    if !ALLOWED_SCHEMES.contains(&url.scheme()) {
        return Err(A2aUrlError::UnsupportedScheme);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(A2aUrlError::CredentialsInUrl);
    }
    if url.host_str().is_none() {
        return Err(A2aUrlError::MissingHost);
    }
    Ok(url)
}
