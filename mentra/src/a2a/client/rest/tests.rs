use std::sync::atomic::AtomicI64;

use reqwest::Method;
use reqwest::header::HeaderMap;
use serde_json::json;

use super::*;
use crate::a2a::A2aBinding;
use crate::a2a::config::A2aLimits;
use crate::a2a::testing::rest_error;

fn transport(endpoint: &str, tenant: Option<&str>) -> Transport {
    Transport {
        http: reqwest::Client::new(),
        binding: A2aBinding::HttpJson,
        endpoint: url::Url::parse(endpoint).expect("endpoint"),
        tenant: tenant.map(str::to_string),
        refuses_credentials_in_clear: false,
        headers: HeaderMap::new(),
        limits: A2aLimits::default(),
        next_id: AtomicI64::new(1),
    }
}

fn operation(verb: Method, path: &[&str]) -> Operation {
    let path = path.iter().map(|segment| segment.to_string());
    Operation::new("GetTask", &json!({}), verb, path).expect("operation")
}

fn url(transport: &Transport, operation: &Operation) -> String {
    transport.rest_url(operation).expect("url").to_string()
}

#[test]
fn paths_go_under_the_interface_url_and_its_tenant() {
    let get = operation(Method::GET, &["tasks", "t1"]);
    for (endpoint, tenant, expected) in [
        ("https://a.example", None, "https://a.example/tasks/t1"),
        (
            "https://a.example/a2a",
            None,
            "https://a.example/a2a/tasks/t1",
        ),
        (
            "https://a.example/a2a/",
            None,
            "https://a.example/a2a/tasks/t1",
        ),
        (
            "https://a.example/a2a",
            Some("acme"),
            "https://a.example/a2a/acme/tasks/t1",
        ),
        (
            "https://a.example",
            Some("a/b c"),
            "https://a.example/a%2Fb%20c/tasks/t1",
        ),
    ] {
        assert_eq!(url(&transport(endpoint, tenant), &get), expected);
    }
}

#[test]
fn an_id_stays_inside_its_segment() {
    let transport = transport("https://a.example", None);
    for (id, expected) in [
        ("a/b", "https://a.example/tasks/a%2Fb"),
        ("a?b#c", "https://a.example/tasks/a%3Fb%23c"),
        ("../x", "https://a.example/tasks/..%2Fx"),
        ("t1:cancel", "https://a.example/tasks/t1:cancel"),
    ] {
        assert_eq!(
            url(&transport, &operation(Method::GET, &["tasks", id])),
            expected
        );
    }
}

#[test]
fn a_segment_the_url_library_would_drop_is_refused() {
    // `Url` drops `.` and `..` rather than appending them, which would turn
    // `GET /tasks/..` into `GET /tasks`, another method.
    for id in ["", ".", ".."] {
        let error = transport("https://a.example", None)
            .rest_url(&operation(Method::GET, &["tasks", id]))
            .expect_err("refused");
        assert!(
            matches!(error, A2aError::InvalidId { method: "GetTask" }),
            "{id:?}"
        );
    }
    let error = transport("https://a.example", Some(".."))
        .rest_url(&operation(Method::GET, &["tasks", "t1"]))
        .expect_err("refused");
    assert!(matches!(error, A2aError::InvalidId { .. }));
}

#[test]
fn a_query_is_encoded_and_only_set_fields_are_sent() {
    let get = operation(Method::GET, &["tasks"])
        .query("contextId", Some("c 1&x=y"))
        .query("pageSize", Some(10))
        .query("pageToken", None::<String>);
    assert_eq!(
        url(&transport("https://a.example", None), &get),
        "https://a.example/tasks?contextId=c+1%26x%3Dy&pageSize=10"
    );
}

#[test]
fn only_a_post_carries_a_body() {
    assert!(operation(Method::POST, &["message:send"]).has_body());
    assert!(!operation(Method::GET, &["tasks"]).has_body());
    assert!(!operation(Method::DELETE, &["tasks", "t1"]).has_body());
}

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
        json!({"error": {"code": 400, "details": [{
            "@type": "type.googleapis.com/google.rpc.ErrorInfo",
            "reason": "TASK_NOT_FOUND",
        }]}}),
        json!({"type": "about:blank", "title": "problem+json is not this binding's shape"}),
    ] {
        assert_eq!(a2a_error(body.to_string().as_bytes()), None, "{body}");
    }
}

#[test]
fn only_an_object_with_a_status_field_is_a_status() {
    let status = |value: serde_json::Value| {
        serde_json::from_value::<Status>(value)
            .expect("any object parses")
            .is_status()
    };
    assert!(status(json!({"code": 404, "status": "NOT_FOUND"})));
    assert!(status(json!({"code": 404, "details": []})));
    assert!(!status(
        json!({"code": -32001, "message": "a JSON-RPC error"})
    ));
}

#[test]
fn a_failure_is_its_a2a_error_or_else_its_status() {
    let method = "GetTask";
    let body = rest_error(-32001).to_string();
    let error = decode_reply(method, StatusCode::NOT_FOUND, body.as_bytes()).expect_err("failed");
    assert!(
        matches!(
            error,
            A2aError::Rpc {
                method: "GetTask",
                error: A2aRpcError::TaskNotFound
            }
        ),
        "{error}"
    );
    assert!(!error.to_string().contains("server text"));

    for body in [&b"<html>gateway</html>"[..], b"", b"{\"error\": {}}"] {
        let error = decode_reply(method, StatusCode::BAD_GATEWAY, body).expect_err("failed");
        assert!(
            matches!(error, A2aError::HttpStatus { status, .. } if status == 502),
            "{error}"
        );
    }
}

#[test]
fn a_success_is_its_json_body() {
    let value = decode_reply("GetTask", StatusCode::OK, br#"{"id": "t1"}"#).expect("json");
    assert_eq!(value, json!({"id": "t1"}));
    let error = decode_reply("GetTask", StatusCode::OK, b"not json").expect_err("not json");
    assert!(matches!(error, A2aError::Decode { method: "GetTask" }));
}

#[test]
fn an_empty_success_is_null() {
    let value = decode_reply("DeleteTaskPushNotificationConfig", StatusCode::OK, b"  \n")
        .expect("empty body");
    assert_eq!(value, Value::Null);
}
