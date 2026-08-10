//! Endpoint URL resolution and same-origin enforcement.
//!
//! The legacy transport lets the *server* name the URL that the client will
//! POST JSON-RPC requests to, by sending it in an `endpoint` event. That makes
//! the endpoint value attacker-controlled whenever the server is compromised,
//! so it is validated before any request — and therefore any configured
//! `Authorization` header — is sent to it.
//!
//! The rule is deliberately strict: the resolved endpoint must share the
//! configured stream URL's scheme, host, and effective port. Anything else is
//! refused rather than normalized, because every relaxation here is a way to
//! redirect credentials to a host the operator never configured.

#[cfg(test)]
mod tests;

use url::Url;

/// Schemes this transport is willing to speak.
const ALLOWED_SCHEMES: [&str; 2] = ["http", "https"];

/// Errors from validating a stream URL or a server-supplied endpoint.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EndpointError {
    #[error("the MCP server sent an empty endpoint event")]
    Empty,

    #[error("could not parse the MCP endpoint URL: {0}")]
    Malformed(String),

    #[error("unsupported MCP endpoint scheme '{scheme}': only http and https are allowed")]
    UnsupportedScheme { scheme: String },

    #[error("the MCP endpoint URL has no host")]
    MissingHost,

    /// Rendered without the credentials themselves so a password in a
    /// misconfigured URL never reaches a log.
    #[error("the MCP endpoint URL must not embed credentials")]
    CredentialsInUrl,

    #[error(
        "the MCP server directed requests to '{endpoint}', which is not the configured origin '{configured}'"
    )]
    CrossOrigin {
        endpoint: String,
        configured: String,
    },
}

/// Validates an operator-configured SSE stream URL.
///
/// This runs before any connection is opened so that a bad configuration fails
/// at the boundary rather than mid-handshake.
pub(crate) fn validate_stream_url(raw: &str) -> Result<Url, EndpointError> {
    let url = Url::parse(raw.trim())
        .map_err(|_| EndpointError::Malformed("invalid URL syntax".to_string()))?;
    check_scheme(&url)?;
    check_no_credentials(&url)?;
    if url.host_str().is_none() {
        return Err(EndpointError::MissingHost);
    }
    Ok(url)
}

/// Resolves a server-supplied endpoint against the stream URL and enforces that
/// it stays on the same origin.
///
/// `raw` is the `data` payload of the `endpoint` event. It is commonly a
/// relative path such as `/messages/?session_id=abc`, but the specification
/// also permits an absolute URL, so both are resolved through [`Url::join`].
pub(crate) fn resolve_endpoint(stream_url: &Url, raw: &str) -> Result<Url, EndpointError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(EndpointError::Empty);
    }

    let endpoint = stream_url
        .join(trimmed)
        .map_err(|_| EndpointError::Malformed("invalid URL syntax".to_string()))?;

    check_scheme(&endpoint)?;
    check_no_credentials(&endpoint)?;
    check_same_origin(stream_url, &endpoint)?;

    Ok(endpoint)
}

/// Rejects any scheme outside the allowlist.
///
/// This also covers `javascript:`, `data:`, and `file:`, which [`Url::join`]
/// happily produces from an absolute URL in the event payload.
fn check_scheme(url: &Url) -> Result<(), EndpointError> {
    if ALLOWED_SCHEMES.contains(&url.scheme()) {
        return Ok(());
    }
    Err(EndpointError::UnsupportedScheme {
        scheme: "[value omitted]".to_string(),
    })
}

/// Rejects a URL carrying userinfo.
///
/// [`Url::origin`] ignores userinfo, so without this check a server could send
/// `https://attacker@configured-host/` and pass the origin comparison while
/// changing what the client transmits.
fn check_no_credentials(url: &Url) -> Result<(), EndpointError> {
    if url.username().is_empty() && url.password().is_none() {
        return Ok(());
    }
    Err(EndpointError::CredentialsInUrl)
}

/// Requires an exact match on scheme, host, and effective port.
///
/// The comparison uses [`Url::port_or_known_default`] so that an explicit
/// default port (`https://host:443`) and an implicit one (`https://host`) are
/// treated as the same origin, and [`Url::host`] rather than the raw string so
/// that equivalent IP literal spellings compare equal. Host names are compared
/// exactly: a trailing dot or a punycode homograph is a different origin.
fn check_same_origin(stream_url: &Url, endpoint: &Url) -> Result<(), EndpointError> {
    let same = stream_url.scheme() == endpoint.scheme()
        && stream_url.host() == endpoint.host()
        && stream_url.port_or_known_default() == endpoint.port_or_known_default();

    if same {
        return Ok(());
    }

    Err(EndpointError::CrossOrigin {
        endpoint: "[server-supplied origin omitted]".to_string(),
        configured: "[configured origin]".to_string(),
    })
}
