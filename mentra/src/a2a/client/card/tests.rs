use serde_json::json;

use super::*;

fn card(requirements: Value) -> Value {
    json!({
        "name": "helper",
        "description": "Answers questions.",
        "version": "1.0.0",
        "supportedInterfaces": [{
            "url": "https://agent.example/a2a",
            "protocolBinding": "JSONRPC",
            "protocolVersion": "1.0"
        }],
        "capabilities": {},
        "defaultInputModes": ["text/plain"],
        "defaultOutputModes": ["text/plain"],
        "skills": [{
            "id": "ask",
            "name": "Ask",
            "description": "Answers.",
            "tags": [],
            "securityRequirements": requirements.clone()
        }],
        "securityRequirements": requirements
    })
}

#[test]
fn a_requirement_whose_empty_scope_list_was_left_out_is_read_as_no_scopes() {
    // What the official Python SDK writes for a bearer token with no scopes.
    let decoded = decode(card(json!([{"schemes": {"bearer": {}}}]))).expect("a valid v1.0 card");

    let expected = Some(vec![
        [("bearer".to_string(), Vec::new())].into_iter().collect(),
    ]);
    assert_eq!(decoded.security_requirements, expected);
    assert_eq!(decoded.skills[0].security_requirements, expected);
}

#[test]
fn scopes_that_are_there_are_kept() {
    let decoded = decode(card(json!([{"schemes": {"oauth": {"list": ["read"]}}}])))
        .expect("a valid v1.0 card");

    assert_eq!(
        decoded.security_requirements,
        Some(vec![
            [("oauth".to_string(), vec!["read".to_string()])]
                .into_iter()
                .collect()
        ])
    );
}

#[test]
fn a_card_without_a_name_is_still_refused() {
    let mut nameless = card(json!([]));
    nameless["name"] = json!(" ");

    assert!(matches!(
        decode(nameless),
        Err(A2aError::InvalidCard("it has no name"))
    ));
}
