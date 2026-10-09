use a2a_lf::{Part, Role, TaskState};
use serde_json::json;

use super::*;
use crate::a2a::testing::{A2aFixture, RpcAnswer, Script, standard_card, task_json};

const SECRET: &str = "sekrit-token-value";

fn config(fixture: &A2aFixture) -> A2aAgentConfig {
    A2aAgentConfig::new("nous", fixture.base()).with_bearer_token(SECRET)
}

fn user_message(text: &str) -> Message {
    Message::new(Role::User, vec![Part::text(text)])
}

fn immediately() -> SendMessageConfiguration {
    SendMessageConfiguration {
        accepted_output_modes: None,
        task_push_notification_config: None,
        history_length: Some(0),
        return_immediately: Some(true),
    }
}

#[tokio::test]
async fn connect_fetches_the_well_known_card_and_selects_its_jsonrpc_interface() {
    let fixture = A2aFixture::start(Script::default());
    let client = A2aClient::connect(&config(&fixture))
        .await
        .expect("connect");

    let requests = fixture.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].target, "/.well-known/agent-card.json");
    assert_eq!(client.card().name, "Nous");
    // The card lists a gRPC interface first; the JSON-RPC one is chosen.
    assert_eq!(
        client.endpoint().as_str(),
        format!("{}/rpc", fixture.base())
    );
}

#[tokio::test]
async fn a_card_without_a_jsonrpc_1_0_interface_is_refused() {
    let mut card = standard_card("http://127.0.0.1:1");
    card["supportedInterfaces"] = json!([
        {"url": "http://127.0.0.1:1/grpc", "protocolBinding": "GRPC", "protocolVersion": "1.0"},
        {"url": "http://127.0.0.1:1/old", "protocolBinding": "JSONRPC", "protocolVersion": "0.3"},
        {"url": "http://127.0.0.1:1/new", "protocolBinding": "JSONRPC", "protocolVersion": "1.1"},
    ]);
    let fixture = A2aFixture::start(Script {
        card: Some(card),
        ..Script::default()
    });

    let error = A2aClient::connect(&config(&fixture))
        .await
        .expect_err("no usable interface");
    assert!(matches!(error, A2aError::NoCompatibleInterface), "{error}");
}

#[test]
fn only_protocol_1_0_at_any_patch_level_is_spoken() {
    for accepted in ["1.0", "1.0.3", "1", " 1.0 "] {
        assert!(speaks_version(accepted), "{accepted}");
    }
    for refused in ["0.3", "1.1", "2.0", "", "one"] {
        assert!(!speaks_version(refused), "{refused}");
    }
}

#[tokio::test]
async fn a_card_that_does_not_parse_is_refused() {
    let fixture = A2aFixture::start(Script {
        card: Some(json!({"name": "no interfaces or version"})),
        ..Script::default()
    });
    let error = A2aClient::connect(&config(&fixture))
        .await
        .expect_err("invalid card");
    assert!(matches!(error, A2aError::InvalidCard(_)), "{error}");
}

#[tokio::test]
async fn every_request_carries_the_version_header_and_the_credentials() {
    let fixture = A2aFixture::start(Script::default());
    let client = A2aClient::connect(&config(&fixture))
        .await
        .expect("connect");
    client.get_task("t1", Some(0)).await.expect("get task");
    client
        .send_message(user_message("hello"), immediately())
        .await
        .expect("send");

    let requests = fixture.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_eq!(request.header("a2a-version"), Some("1.0"));
        assert_eq!(
            request.header("authorization"),
            Some(format!("Bearer {SECRET}").as_str())
        );
    }
    assert_eq!(fixture.rpc_methods(), ["GetTask", "SendMessage"]);
}

#[tokio::test]
async fn send_message_puts_the_configuration_and_message_on_the_wire() {
    let fixture = A2aFixture::start(Script::default());
    let client = A2aClient::connect(&config(&fixture))
        .await
        .expect("connect");

    let response = client
        .send_message(user_message("hello"), immediately())
        .await
        .expect("send");
    assert!(matches!(response, SendMessageResponse::Task(task) if task.id == "t1"));

    let params = &fixture.params_of("SendMessage")[0];
    assert_eq!(params["configuration"]["returnImmediately"], json!(true));
    assert_eq!(params["configuration"]["historyLength"], json!(0));
    assert_eq!(params["message"]["role"], json!("ROLE_USER"));
    assert_eq!(params["message"]["parts"][0]["text"], json!("hello"));
}

#[tokio::test]
async fn a_plaintext_endpoint_named_by_the_card_is_refused_when_credentials_are_configured() {
    let mut card = standard_card("http://127.0.0.1:1");
    card["supportedInterfaces"] = json!([
        {"url": "http://agent.example.com/rpc", "protocolBinding": "JSONRPC", "protocolVersion": "1.0"},
    ]);
    let fixture = A2aFixture::start(Script {
        card: Some(card),
        ..Script::default()
    });

    let error = A2aClient::connect(&config(&fixture))
        .await
        .expect_err("plaintext endpoint refused");
    assert!(matches!(error, A2aError::PlaintextEndpoint), "{error}");

    // With no credential to protect, or with the override, it is allowed.
    A2aClient::connect(&A2aAgentConfig::new("nous", fixture.base()))
        .await
        .expect("no credentials");
    A2aClient::connect(&config(&fixture).allowing_plaintext_credentials())
        .await
        .expect("override");
}

#[tokio::test]
async fn plaintext_credentials_are_refused_before_any_request() {
    let config = A2aAgentConfig::new("nous", "http://agent.example.com").with_bearer_token(SECRET);
    let error = A2aClient::connect(&config).await.expect_err("refused");
    assert!(matches!(
        error,
        A2aError::Config(A2aConfigError::PlaintextCredentials { .. })
    ));
    assert!(!error.to_string().contains(SECRET));
}

#[tokio::test]
async fn jsonrpc_errors_map_to_typed_codes_without_server_text() {
    let fixture = A2aFixture::start(Script {
        get: vec![RpcAnswer::Error(-32001)],
        cancel: RpcAnswer::Error(-32002),
        ..Script::default()
    });
    let client = A2aClient::connect(&config(&fixture))
        .await
        .expect("connect");

    let error = client.get_task("t9", None).await.expect_err("not found");
    assert!(matches!(
        error,
        A2aError::Rpc {
            method: "GetTask",
            error: A2aRpcError::TaskNotFound
        }
    ));
    let text = error.to_string();
    assert!(text.contains("TaskNotFoundError (-32001)"), "{text}");
    assert!(!text.contains("server text"), "{text}");

    let error = client.cancel_task("t1").await.expect_err("not cancelable");
    assert!(matches!(
        error,
        A2aError::Rpc {
            error: A2aRpcError::TaskNotCancelable,
            ..
        }
    ));
}

#[tokio::test]
async fn an_oversized_reply_is_bounded() {
    let fixture = A2aFixture::start(Script {
        get: vec![RpcAnswer::Http(200, "x".repeat(64 * 1024))],
        ..Script::default()
    });
    let limits = A2aLimits {
        max_response_bytes: 4 * 1024,
        ..A2aLimits::default()
    };
    let client = A2aClient::connect(&config(&fixture).with_limits(limits))
        .await
        .expect("connect");

    let error = client.get_task("t1", None).await.expect_err("too large");
    assert!(
        matches!(error, A2aError::ResponseTooLarge { limit: 4096 }),
        "{error}"
    );
}

#[tokio::test]
async fn http_failures_never_show_credentials_or_server_bodies() {
    let fixture = A2aFixture::start(Script {
        get: vec![RpcAnswer::Http(401, format!("bad token {SECRET}"))],
        cancel: RpcAnswer::Http(302, String::new()),
        ..Script::default()
    });
    let client = A2aClient::connect(&config(&fixture))
        .await
        .expect("connect");

    let error = client.get_task("t1", None).await.expect_err("unauthorized");
    assert!(matches!(error, A2aError::HttpStatus { status, .. } if status == 401));
    let error = client.cancel_task("t1").await.expect_err("redirect");
    assert!(matches!(error, A2aError::RedirectRefused));

    for text in [
        error.to_string(),
        format!("{error:?}"),
        format!("{client:?}"),
        client
            .get_task("t1", None)
            .await
            .expect_err("unauthorized")
            .to_string(),
    ] {
        assert!(!text.contains(SECRET), "{text}");
    }
}

#[tokio::test]
async fn a_send_that_may_have_been_accepted_is_indeterminate() {
    for (answer, indeterminate) in [
        (RpcAnswer::Http(502, "bad gateway".to_string()), true),
        (RpcAnswer::Http(200, "not json".to_string()), true),
        (RpcAnswer::Http(400, String::new()), false),
        (RpcAnswer::Error(-32602), false),
    ] {
        let fixture = A2aFixture::start(Script {
            send: answer.clone(),
            ..Script::default()
        });
        let client = A2aClient::connect(&config(&fixture))
            .await
            .expect("connect");
        let error = client
            .send_message(user_message("hello"), immediately())
            .await
            .expect_err("send fails");
        assert_eq!(
            matches!(error, A2aError::SendIndeterminate),
            indeterminate,
            "{answer:?} -> {error}"
        );
    }
}

#[tokio::test]
async fn an_answer_about_a_different_task_is_refused() {
    let fixture = A2aFixture::start(Script {
        get: vec![RpcAnswer::Result(task_json(
            "other",
            "c1",
            "TASK_STATE_WORKING",
        ))],
        ..Script::default()
    });
    let client = A2aClient::connect(&config(&fixture))
        .await
        .expect("connect");

    let error = client.get_task("t1", None).await.expect_err("mismatch");
    assert!(matches!(
        error,
        A2aError::MismatchedTask { method: "GetTask" }
    ));
}

#[tokio::test]
async fn get_task_decodes_state_and_artifacts() {
    let mut task = task_json("t1", "c1", "TASK_STATE_COMPLETED");
    task["artifacts"] = json!([{"artifactId": "a1", "parts": [{"text": "the answer"}]}]);
    let fixture = A2aFixture::start(Script {
        get: vec![RpcAnswer::Result(task)],
        ..Script::default()
    });
    let client = A2aClient::connect(&config(&fixture))
        .await
        .expect("connect");

    let task = client.get_task("t1", Some(0)).await.expect("get");
    assert_eq!(task.status.state, TaskState::Completed);
    assert_eq!(task.context_id, "c1");
    let artifacts = task.artifacts.expect("artifacts");
    assert_eq!(artifacts[0].parts[0].as_text(), Some("the answer"));
    assert_eq!(fixture.params_of("GetTask")[0]["historyLength"], json!(0));
}
