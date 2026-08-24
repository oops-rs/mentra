//! Tests for SSE server configuration and secret redaction.
//!
//! [`SecretString`](crate::mcp::secret::SecretString) itself is covered in its
//! own module; these cases assert that this configuration inherits its
//! redaction.

use super::{McpSseConfigError, McpSseServerConfig};

// ---------------------------------------------------------------------------
// Secret redaction
// ---------------------------------------------------------------------------

#[test]
fn config_debug_redacts_header_values_but_keeps_names() {
    let config = McpSseServerConfig::new("obs", "https://mcp.example.com/sse")
        .with_header("authorization", "Bearer super-secret-token")
        .with_header("x-tenant", "acme");
    let rendered = format!("{config:?}");

    assert!(
        !rendered.contains("super-secret-token"),
        "the token must not appear: {rendered}"
    );
    assert!(
        !rendered.contains("acme"),
        "no header value may appear: {rendered}"
    );
    assert!(
        rendered.contains("authorization"),
        "header names stay visible for diagnosis: {rendered}"
    );
    assert!(rendered.contains("x-tenant"), "got {rendered}");
    assert!(rendered.contains("mcp.example.com"), "got {rendered}");
}

#[test]
fn config_alternate_debug_redacts_header_values() {
    let config = McpSseServerConfig::new("obs", "https://mcp.example.com/sse")
        .with_bearer_token("super-secret-token");
    let rendered = format!("{config:#?}");
    assert!(!rendered.contains("super-secret-token"), "got {rendered}");
}

#[test]
fn a_bearer_token_is_stored_as_an_authorization_header() {
    let config =
        McpSseServerConfig::new("obs", "https://mcp.example.com/sse").with_bearer_token("abc123");
    let value = config
        .headers
        .get("authorization")
        .expect("bearer token sets the authorization header");
    assert_eq!(value.expose_secret(), "Bearer abc123");
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

#[test]
fn accepts_an_https_url_with_headers() {
    let config =
        McpSseServerConfig::new("obs", "https://mcp.example.com/sse").with_bearer_token("abc123");
    let url = config.validate().expect("https with headers is allowed");
    assert_eq!(url.host_str(), Some("mcp.example.com"));
}

#[test]
fn accepts_a_plain_http_url_without_headers() {
    let config = McpSseServerConfig::new("local", "http://internal.corp:8080/sse");
    config
        .validate()
        .expect("plaintext without credentials is allowed");
}

#[test]
fn rejects_plaintext_credentials_to_a_remote_host() {
    let config =
        McpSseServerConfig::new("obs", "http://internal.corp/sse").with_bearer_token("abc123");
    let error = config
        .validate()
        .expect_err("a token must not cross the network in the clear");
    assert!(matches!(
        error,
        McpSseConfigError::PlaintextCredentials { .. }
    ));
}

#[test]
fn allows_plaintext_credentials_to_localhost() {
    let config =
        McpSseServerConfig::new("local", "http://localhost:3000/sse").with_bearer_token("abc123");
    config
        .validate()
        .expect("loopback never leaves the machine");
}

#[test]
fn allows_plaintext_credentials_to_the_ipv4_loopback_address() {
    let config =
        McpSseServerConfig::new("local", "http://127.0.0.1:3000/sse").with_bearer_token("abc");
    config.validate().expect("127.0.0.1 is loopback");
}

#[test]
fn allows_plaintext_credentials_to_the_ipv6_loopback_address() {
    let config = McpSseServerConfig::new("local", "http://[::1]:3000/sse").with_bearer_token("abc");
    config.validate().expect("::1 is loopback");
}

#[test]
fn allows_plaintext_credentials_when_explicitly_opted_in() {
    let config = McpSseServerConfig::new("obs", "http://internal.corp/sse")
        .with_bearer_token("abc123")
        .allowing_plaintext_credentials();
    config
        .validate()
        .expect("the operator may override deliberately");
}

#[test]
fn rejects_an_empty_server_name() {
    let config = McpSseServerConfig::new("   ", "https://mcp.example.com/sse");
    let error = config.validate().expect_err("a name is required");
    assert!(matches!(error, McpSseConfigError::EmptyName));
}

#[test]
fn rejects_an_unsupported_url_scheme() {
    let config = McpSseServerConfig::new("obs", "ws://mcp.example.com/sse");
    let error = config
        .validate()
        .expect_err("only http and https are allowed");
    assert!(matches!(error, McpSseConfigError::Url(_)));
}

#[test]
fn rejects_a_url_with_embedded_credentials() {
    let config = McpSseServerConfig::new("obs", "https://user:pass@mcp.example.com/sse");
    let error = config
        .validate()
        .expect_err("credentials belong in headers, not the URL");
    assert!(matches!(error, McpSseConfigError::Url(_)));
}

#[test]
fn rejects_a_header_name_that_is_not_valid_for_http() {
    let config = McpSseServerConfig::new("obs", "https://mcp.example.com/sse")
        .with_header("bad header", "value");
    let error = config.validate().expect_err("header names are validated");
    assert!(matches!(error, McpSseConfigError::InvalidHeaderName { .. }));
}

#[test]
fn rejects_a_header_value_that_is_not_valid_for_http() {
    let config = McpSseServerConfig::new("obs", "https://mcp.example.com/sse")
        .with_header("authorization", "Bearer \nInjected: header");
    let error = config.validate().expect_err("header values are validated");
    assert!(matches!(
        error,
        McpSseConfigError::InvalidHeaderValue { .. }
    ));
}

#[test]
fn the_invalid_header_value_error_does_not_echo_the_value() {
    let config = McpSseServerConfig::new("obs", "https://mcp.example.com/sse")
        .with_header("authorization", "Bearer \nsuper-secret");
    let error = config.validate().expect_err("header values are validated");
    let rendered = error.to_string();
    assert!(
        !rendered.contains("super-secret"),
        "the error must not echo a secret: {rendered}"
    );
    assert!(rendered.contains("authorization"), "got {rendered}");
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

#[test]
fn default_limits_match_the_stdio_client_where_the_concept_is_shared() {
    let limits = McpSseServerConfig::new("obs", "https://mcp.example.com/sse").limits;
    assert_eq!(
        limits.initialize_timeout,
        std::time::Duration::from_secs(10)
    );
    assert_eq!(
        limits.list_tools_timeout,
        std::time::Duration::from_secs(30)
    );
    assert_eq!(
        limits.call_tool_timeout,
        std::time::Duration::from_secs(120)
    );
}

#[test]
fn the_endpoint_limit_is_tighter_than_the_general_event_limit() {
    let limits = McpSseServerConfig::new("obs", "https://mcp.example.com/sse").limits;
    assert!(
        limits.max_endpoint_bytes < limits.max_event_bytes,
        "the pre-correlation surface must be smaller"
    );
}

#[test]
fn a_config_deserializes_from_json_without_limits() {
    let config: McpSseServerConfig = serde_json::from_value(serde_json::json!({
        "name": "obs",
        "url": "https://mcp.example.com/sse",
        "headers": {"authorization": "Bearer abc123"}
    }))
    .expect("deserialize");

    assert_eq!(config.name, "obs");
    assert_eq!(
        config
            .headers
            .get("authorization")
            .expect("header")
            .expose_secret(),
        "Bearer abc123"
    );
    assert_eq!(config.limits, super::McpSseLimits::default());
}
