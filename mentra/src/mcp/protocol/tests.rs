use std::collections::HashMap;

use super::McpServerConfig;

/// Builds a stdio configuration whose environment carries a credential.
fn config_with_token() -> McpServerConfig {
    McpServerConfig {
        name: "github".to_string(),
        command: "npx".to_string(),
        args: vec![
            "-y".to_string(),
            "@modelcontextprotocol/server-github".to_string(),
        ],
        env: HashMap::from([
            (
                "GITHUB_TOKEN".to_string(),
                "ghp_supersecretvalue".to_string(),
            ),
            ("PATH".to_string(), "/usr/bin".to_string()),
        ]),
        cwd: None,
    }
}

#[test]
fn debug_hides_environment_values() {
    let rendered = format!("{:?}", config_with_token());

    assert!(
        !rendered.contains("ghp_supersecretvalue"),
        "the token leaked into Debug output: {rendered}"
    );
    assert!(
        !rendered.contains("/usr/bin"),
        "every environment value must be redacted, not only the ones that look secret: {rendered}"
    );
}

#[test]
fn debug_keeps_environment_names() {
    let rendered = format!("{:?}", config_with_token());

    assert!(
        rendered.contains("GITHUB_TOKEN"),
        "the variable name is the operator-useful half: {rendered}"
    );
    assert!(rendered.contains("PATH"), "{rendered}");
}

#[test]
fn debug_keeps_the_non_secret_fields() {
    let rendered = format!("{:?}", config_with_token());

    assert!(rendered.contains("github"), "{rendered}");
    assert!(rendered.contains("npx"), "{rendered}");
    assert!(
        rendered.contains("@modelcontextprotocol/server-github"),
        "{rendered}"
    );
}

#[test]
fn debug_renders_environment_names_in_a_stable_order() {
    let config = config_with_token();

    let first = format!("{config:?}");
    for _ in 0..32 {
        assert_eq!(
            format!("{config:?}"),
            first,
            "HashMap iteration order reached Debug output"
        );
    }
}

#[test]
fn serialize_still_carries_environment_values() {
    let json = serde_json::to_value(config_with_token()).expect("serialize the config");

    assert_eq!(json["env"]["GITHUB_TOKEN"], "ghp_supersecretvalue");
}

#[test]
fn round_trips_through_serde() {
    let config = config_with_token();
    let json = serde_json::to_string(&config).expect("serialize the config");
    let restored: McpServerConfig = serde_json::from_str(&json).expect("deserialize the config");

    assert_eq!(restored, config);
}
