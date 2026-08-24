//! Tests for credential redaction.

use super::SecretString;

#[test]
fn secret_debug_output_hides_the_value() {
    let secret = SecretString::new("Bearer super-secret-token");
    let rendered = format!("{secret:?}");
    assert!(!rendered.contains("super-secret-token"), "got {rendered}");
    assert_eq!(rendered, "SecretString([redacted])");
}

#[test]
fn secret_alternate_debug_output_hides_the_value() {
    let secret = SecretString::new("Bearer super-secret-token");
    let rendered = format!("{secret:#?}");
    assert!(!rendered.contains("super-secret-token"), "got {rendered}");
}

#[test]
fn exposing_a_secret_returns_the_original_value() {
    let secret = SecretString::new("Bearer token");
    assert_eq!(secret.expose_secret(), "Bearer token");
}

#[test]
fn deserializes_transparently_from_a_bare_string() {
    let secret: SecretString =
        serde_json::from_value(serde_json::json!("Bearer token")).expect("deserialize the secret");

    assert_eq!(secret.expose_secret(), "Bearer token");
}
