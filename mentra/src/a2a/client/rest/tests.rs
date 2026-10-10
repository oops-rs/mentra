use serde_json::json;

use super::*;
use crate::a2a::testing::rest_error;

#[test]
fn a_status_names_its_a2a_error_by_reason_and_domain() {
    for code in [-32001, -32002, -32004, -32007, -32008, -32009] {
        let body = rest_error(code).to_string();
        assert_eq!(
            a2a_error(body.as_bytes()).map(A2aRpcError::code),
            Some(code)
        );
    }
}

#[test]
fn a_status_without_an_a2a_error_info_names_nothing() {
    for body in [
        json!({"error": {"code": 400, "message": "bad"}}),
        json!({"error": {"code": 400, "details": [{
            "@type": "type.googleapis.com/google.rpc.BadRequest",
            "reason": "TASK_NOT_FOUND",
            "domain": "a2a-protocol.org",
        }]}}),
        json!({"error": {"code": 400, "details": [{
            "@type": "type.googleapis.com/google.rpc.ErrorInfo",
            "reason": "SOMETHING_ELSE",
            "domain": "a2a-protocol.org",
        }]}}),
        json!({"type": "about:blank", "title": "problem+json is not this binding's shape"}),
    ] {
        assert_eq!(a2a_error(body.to_string().as_bytes()), None, "{body}");
    }
}

#[test]
fn an_empty_success_is_null() {
    let value = decode_reply("DeleteTaskPushNotificationConfig", StatusCode::OK, b"  \n")
        .expect("empty body");
    assert_eq!(value, Value::Null);
}
