//! Tests for endpoint URL resolution and same-origin enforcement.

use url::Url;

use super::{EndpointError, resolve_endpoint, validate_stream_url};

/// The configured SSE URL every test resolves against.
const BASE: &str = "https://good-host.example/sse";

fn base() -> Url {
    Url::parse(BASE).expect("base URL should parse")
}

fn resolve(raw: &str) -> Result<Url, EndpointError> {
    resolve_endpoint(&base(), raw)
}

// ---------------------------------------------------------------------------
// Accepted endpoints
// ---------------------------------------------------------------------------

#[test]
fn resolves_an_absolute_path_against_the_stream_url() {
    let endpoint = resolve("/messages/?session_id=abc").expect("same-origin path is allowed");
    assert_eq!(
        endpoint.as_str(),
        "https://good-host.example/messages/?session_id=abc"
    );
}

#[test]
fn resolves_a_relative_path_against_the_stream_url() {
    let base = Url::parse("https://good-host.example/mcp/sse").expect("base should parse");
    let endpoint = resolve_endpoint(&base, "messages?session_id=abc").expect("relative is allowed");
    assert_eq!(
        endpoint.as_str(),
        "https://good-host.example/mcp/messages?session_id=abc"
    );
}

#[test]
fn accepts_an_absolute_url_on_the_same_origin() {
    let endpoint =
        resolve("https://good-host.example/messages/").expect("same-origin absolute is allowed");
    assert_eq!(endpoint.as_str(), "https://good-host.example/messages/");
}

#[test]
fn accepts_an_explicit_default_port_matching_the_implicit_one() {
    // https://host and https://host:443 are the same origin.
    let endpoint = resolve("https://good-host.example:443/messages/")
        .expect("default port is the same origin");
    assert_eq!(endpoint.as_str(), "https://good-host.example/messages/");
}

#[test]
fn accepts_an_implicit_default_port_matching_an_explicit_one() {
    let base = Url::parse("https://good-host.example:443/sse").expect("base should parse");
    let endpoint = resolve_endpoint(&base, "https://good-host.example/messages/")
        .expect("implicit port is the same origin");
    assert_eq!(endpoint.as_str(), "https://good-host.example/messages/");
}

#[test]
fn accepts_a_matching_non_default_port() {
    let base = Url::parse("http://127.0.0.1:8080/sse").expect("base should parse");
    let endpoint =
        resolve_endpoint(&base, "/messages/?session_id=abc").expect("same port is allowed");
    assert_eq!(
        endpoint.as_str(),
        "http://127.0.0.1:8080/messages/?session_id=abc"
    );
}

#[test]
fn accepts_a_host_differing_only_by_case() {
    // Host comparison is case-insensitive because the parser normalizes it.
    let endpoint =
        resolve("https://GOOD-HOST.EXAMPLE/messages/").expect("host case is not significant");
    assert_eq!(endpoint.as_str(), "https://good-host.example/messages/");
}

#[test]
fn accepts_a_plain_http_origin_when_the_stream_is_plain_http() {
    let base = Url::parse("http://localhost:3000/sse").expect("base should parse");
    let endpoint = resolve_endpoint(&base, "/messages/").expect("http is an allowed scheme");
    assert_eq!(endpoint.as_str(), "http://localhost:3000/messages/");
}

#[test]
fn preserves_the_query_string_carrying_the_session_id() {
    let endpoint = resolve("/messages/?session_id=6c8f2a&foo=bar").expect("query is preserved");
    assert_eq!(endpoint.query(), Some("session_id=6c8f2a&foo=bar"));
}

// ---------------------------------------------------------------------------
// Rejected endpoints — cross-origin
// ---------------------------------------------------------------------------

#[test]
fn rejects_an_absolute_url_on_a_different_host() {
    let error = resolve("https://evil.example/steal").expect_err("cross-host must be rejected");
    assert!(matches!(error, EndpointError::CrossOrigin { .. }));
}

#[test]
fn rejects_a_protocol_relative_url_that_replaces_the_authority() {
    // `//evil.example/x` inherits only the scheme; url::join gives it a NEW host.
    let error = resolve("//evil.example/steal").expect_err("protocol-relative must be rejected");
    assert!(matches!(error, EndpointError::CrossOrigin { .. }));
}

#[test]
fn rejects_a_backslash_authority_that_url_normalizes_to_a_new_host() {
    // url normalizes leading backslashes the way browsers do, yielding a new host.
    let error = resolve("/\\evil.example/steal").expect_err("backslash authority must be rejected");
    assert!(matches!(error, EndpointError::CrossOrigin { .. }));
}

#[test]
fn rejects_a_scheme_downgrade_to_plain_http() {
    let error =
        resolve("http://good-host.example/messages/").expect_err("downgrade must be rejected");
    assert!(matches!(error, EndpointError::CrossOrigin { .. }));
}

#[test]
fn rejects_a_scheme_upgrade_to_https() {
    let base = Url::parse("http://good-host.example/sse").expect("base should parse");
    let error = resolve_endpoint(&base, "https://good-host.example/messages/")
        .expect_err("scheme change must be rejected");
    assert!(matches!(error, EndpointError::CrossOrigin { .. }));
}

#[test]
fn rejects_a_different_explicit_port() {
    let error =
        resolve("https://good-host.example:8443/messages/").expect_err("port change is rejected");
    assert!(matches!(error, EndpointError::CrossOrigin { .. }));
}

#[test]
fn rejects_a_trailing_dot_host_that_resolves_to_the_same_name() {
    // `good-host.example.` is a distinct host string; treat it as cross-origin
    // rather than guessing at DNS equivalence.
    let error =
        resolve("https://good-host.example./messages/").expect_err("trailing dot is rejected");
    assert!(matches!(error, EndpointError::CrossOrigin { .. }));
}

#[test]
fn rejects_a_punycode_homograph_host() {
    let error = resolve("https://g\u{f6}\u{f6}d-host.example/messages/")
        .expect_err("homograph host is rejected");
    assert!(matches!(error, EndpointError::CrossOrigin { .. }));
}

#[test]
fn rejects_a_subdomain_of_the_configured_host() {
    let error = resolve("https://evil.good-host.example/messages/")
        .expect_err("subdomains are a different origin");
    assert!(matches!(error, EndpointError::CrossOrigin { .. }));
}

#[test]
fn rejects_a_suffix_extension_of_the_configured_host() {
    let error = resolve("https://good-host.example.evil.test/messages/")
        .expect_err("suffix extension is a different origin");
    assert!(matches!(error, EndpointError::CrossOrigin { .. }));
}

// ---------------------------------------------------------------------------
// Rejected endpoints — credentials and schemes
// ---------------------------------------------------------------------------

#[test]
fn rejects_userinfo_even_on_the_matching_origin() {
    // url::Origin ignores userinfo, so an explicit check is required: credentials
    // in the URL would be sent to the server alongside the configured headers.
    let error = resolve("https://attacker@good-host.example/messages/")
        .expect_err("userinfo must be rejected");
    assert!(matches!(error, EndpointError::CredentialsInUrl));
}

#[test]
fn rejects_a_password_in_the_endpoint_url() {
    let error = resolve("https://user:secret@good-host.example/messages/")
        .expect_err("password must be rejected");
    assert!(matches!(error, EndpointError::CredentialsInUrl));
}

#[test]
fn rejects_a_javascript_scheme() {
    let error = resolve("javascript:alert(1)").expect_err("javascript must be rejected");
    assert!(matches!(error, EndpointError::UnsupportedScheme { .. }));
}

#[test]
fn rejects_a_data_scheme() {
    let error = resolve("data:text/plain,hi").expect_err("data must be rejected");
    assert!(matches!(error, EndpointError::UnsupportedScheme { .. }));
}

#[test]
fn rejects_a_file_scheme() {
    let error = resolve("file:///etc/passwd").expect_err("file must be rejected");
    assert!(matches!(error, EndpointError::UnsupportedScheme { .. }));
}

// ---------------------------------------------------------------------------
// Rejected endpoints — malformed
// ---------------------------------------------------------------------------

#[test]
fn rejects_an_empty_endpoint_payload() {
    let error = resolve("").expect_err("an empty endpoint must be rejected");
    assert!(matches!(error, EndpointError::Empty));
}

#[test]
fn rejects_a_whitespace_only_endpoint_payload() {
    let error = resolve("   ").expect_err("a blank endpoint must be rejected");
    assert!(matches!(error, EndpointError::Empty));
}

#[test]
fn rejects_an_unparseable_endpoint() {
    let error = resolve("http://[not-an-address/x").expect_err("garbage must be rejected");
    assert!(matches!(error, EndpointError::Malformed(_)));
}

#[test]
fn trims_surrounding_whitespace_before_resolving() {
    // Servers occasionally pad the data field; trimming must happen before the
    // origin check so it cannot be used to smuggle a different authority.
    let endpoint = resolve("  /messages/?session_id=abc  ").expect("padding is trimmed");
    assert_eq!(
        endpoint.as_str(),
        "https://good-host.example/messages/?session_id=abc"
    );
}

#[test]
fn rejects_a_padded_cross_origin_endpoint() {
    let error =
        resolve("  https://evil.example/steal  ").expect_err("padding does not bypass the check");
    assert!(matches!(error, EndpointError::CrossOrigin { .. }));
}

// ---------------------------------------------------------------------------
// Stream URL validation
// ---------------------------------------------------------------------------

#[test]
fn accepts_an_https_stream_url() {
    let url = validate_stream_url("https://good-host.example/sse").expect("https is allowed");
    assert_eq!(url.scheme(), "https");
}

#[test]
fn accepts_an_http_stream_url() {
    let url = validate_stream_url("http://127.0.0.1:9000/sse").expect("http is allowed");
    assert_eq!(url.scheme(), "http");
}

#[test]
fn rejects_a_stream_url_with_an_unsupported_scheme() {
    let error = validate_stream_url("ws://good-host.example/sse").expect_err("ws is rejected");
    assert!(matches!(error, EndpointError::UnsupportedScheme { .. }));
}

#[test]
fn rejects_a_stream_url_with_embedded_credentials() {
    let error = validate_stream_url("https://user:pass@good-host.example/sse")
        .expect_err("credentials are rejected");
    assert!(matches!(error, EndpointError::CredentialsInUrl));
}

#[test]
fn rejects_a_stream_url_without_a_host() {
    let error = validate_stream_url("file:///tmp/sse").expect_err("a hostless URL is rejected");
    assert!(matches!(
        error,
        EndpointError::UnsupportedScheme { .. } | EndpointError::MissingHost
    ));
}

#[test]
fn rejects_an_unparseable_stream_url() {
    let error = validate_stream_url("not a url").expect_err("garbage is rejected");
    assert!(matches!(error, EndpointError::Malformed(_)));
}

// ---------------------------------------------------------------------------
// Error reporting
// ---------------------------------------------------------------------------

#[test]
fn the_cross_origin_error_does_not_retain_either_origin() {
    let error =
        resolve("https://remote-canary.invalid/steal").expect_err("cross-origin is rejected");
    let rendered = error.to_string();
    let debug = format!("{error:?}");
    for origin in ["remote-canary.invalid", "good-host.example"] {
        assert!(!rendered.contains(origin), "got {rendered}");
        assert!(!debug.contains(origin), "got {debug}");
    }
}

#[test]
fn unsupported_scheme_errors_do_not_retain_the_scheme() {
    let error = resolve("remote-canary:payload").expect_err("the scheme is unsupported");
    let rendered = error.to_string();
    let debug = format!("{error:?}");
    assert!(!rendered.contains("remote-canary"), "got {rendered}");
    assert!(!debug.contains("remote-canary"), "got {debug}");
}

#[test]
fn malformed_endpoint_errors_do_not_retain_the_payload() {
    let error = resolve("http://[remote-canary.invalid").expect_err("the endpoint is malformed");
    let rendered = error.to_string();
    let debug = format!("{error:?}");
    assert!(!rendered.contains("remote-canary"), "got {rendered}");
    assert!(!debug.contains("remote-canary"), "got {debug}");
}

#[test]
fn the_credentials_error_does_not_echo_the_credentials() {
    let error = resolve("https://user:hunter2@good-host.example/messages/")
        .expect_err("credentials are rejected");
    let rendered = error.to_string();
    assert!(
        !rendered.contains("hunter2"),
        "the error must not echo a secret: {rendered}"
    );
}
