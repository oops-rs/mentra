use super::*;

#[test]
fn a_base_url_resolves_to_the_well_known_card() {
    for base in [
        "https://nous.example.com",
        "https://nous.example.com/",
        "http://127.0.0.1:8080",
    ] {
        let url = A2aAgentConfig::new("nous", base).card_url().expect("valid");
        assert_eq!(url.path(), WELL_KNOWN_AGENT_CARD_PATH, "{base}");
    }
}

#[test]
fn a_url_with_a_path_or_query_is_the_card_itself() {
    for card in [
        "https://example.com/agents/nous/card.json",
        "https://example.com/.well-known/agent-card.json",
        "https://example.com/?agent=nous",
    ] {
        let url = A2aAgentConfig::new("nous", card).card_url().expect("valid");
        assert_eq!(url.as_str(), card);
    }
}

#[test]
fn invalid_names_are_refused_with_the_mcp_rules() {
    for name in ["", "evil__foo", "evil_"] {
        assert!(matches!(
            A2aAgentConfig::new(name, "https://example.com").validate(),
            Err(A2aConfigError::InvalidName(_))
        ));
    }
}

#[test]
fn credentials_over_plaintext_to_a_remote_host_are_refused() {
    let config = A2aAgentConfig::new("nous", "http://nous.example.com").with_bearer_token("sekrit");
    let error = config.validate().expect_err("plaintext refused");
    assert!(matches!(error, A2aConfigError::PlaintextCredentials { .. }));
    assert!(!error.to_string().contains("sekrit"));

    // Loopback never leaves the machine; https is fine; the override works;
    // and with no header there is nothing to protect.
    A2aAgentConfig::new("nous", "http://localhost:9000")
        .with_bearer_token("sekrit")
        .validate()
        .expect("loopback allowed");
    A2aAgentConfig::new("nous", "https://nous.example.com")
        .with_bearer_token("sekrit")
        .validate()
        .expect("https allowed");
    config
        .allowing_plaintext_credentials()
        .validate()
        .expect("override honored");
    A2aAgentConfig::new("nous", "http://nous.example.com")
        .validate()
        .expect("no credentials to protect");
}

#[test]
fn reserved_and_malformed_headers_are_refused_without_their_values() {
    let reserved =
        A2aAgentConfig::new("nous", "https://example.com").with_header("A2A-Version", "0.3");
    assert!(matches!(
        reserved.validate(),
        Err(A2aConfigError::ReservedHeader { .. })
    ));

    let malformed =
        A2aAgentConfig::new("nous", "https://example.com").with_header("x-key", "sek\nrit");
    let error = malformed.validate().expect_err("invalid value");
    assert!(matches!(error, A2aConfigError::InvalidHeaderValue { .. }));
    assert!(!error.to_string().contains("sek"));
}

#[test]
fn urls_outside_http_or_carrying_credentials_are_refused() {
    for (url, expected) in [
        ("not a url", A2aUrlError::Malformed),
        ("file:///etc/passwd", A2aUrlError::UnsupportedScheme),
        ("https://user:pw@example.com", A2aUrlError::CredentialsInUrl),
    ] {
        assert_eq!(
            A2aAgentConfig::new("nous", url).validate(),
            Err(A2aConfigError::Url(expected)),
            "{url}"
        );
    }
}

#[test]
fn zero_limits_that_would_spin_or_starve_are_refused() {
    let limits = A2aLimits {
        poll_interval: Duration::ZERO,
        ..A2aLimits::default()
    };
    assert_eq!(
        A2aAgentConfig::new("nous", "https://example.com")
            .with_limits(limits)
            .validate(),
        Err(A2aConfigError::ZeroLimit {
            name: "poll_interval"
        })
    );
}

#[test]
fn debug_output_never_shows_a_header_value() {
    let config = A2aAgentConfig::new("nous", "https://example.com").with_bearer_token("sekrit");
    assert!(!format!("{config:?}").contains("sekrit"));
}

#[test]
fn deserializes_the_operator_facing_fields() {
    let config: A2aAgentConfig = serde_json::from_value(serde_json::json!({
        "name": "nous",
        "url": "https://nous.example.com",
        "headers": {"authorization": "Bearer sekrit"},
    }))
    .expect("deserialize");
    assert_eq!(config.name, "nous");
    assert!(!config.allow_plaintext_credentials);
    assert_eq!(config.limits, A2aLimits::default());
    assert_eq!(
        config.headers["authorization"].expose_secret(),
        "Bearer sekrit"
    );
}
