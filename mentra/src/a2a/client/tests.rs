use a2a_lf::{Part, Role, TaskState};
use serde_json::json;

use super::binding::speaks_version;
use super::*;
use crate::a2a::A2aRpcError;
use crate::a2a::config::A2aConfigError;
use crate::a2a::testing::{A2aFixture, RpcAnswer, Script, standard_card, task_json};

const SECRET: &str = "sekrit-token-value";

/// Both bindings, for the tests every method must pass over each.
const BINDINGS: [A2aBinding; 2] = [A2aBinding::JsonRpc, A2aBinding::HttpJson];

fn config(fixture: &A2aFixture) -> A2aAgentConfig {
    A2aAgentConfig::new("nous", fixture.base()).with_bearer_token(SECRET)
}

async fn connect(fixture: &A2aFixture) -> A2aClient {
    A2aClient::connect(&config(fixture)).await.expect("connect")
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

/// The path and query of every request after the card fetch.
fn targets(fixture: &A2aFixture) -> Vec<String> {
    fixture.requests()[1..]
        .iter()
        .map(|request| format!("{} {}", request.method, request.target))
        .collect()
}

#[tokio::test]
async fn connect_selects_the_first_interface_it_speaks_in_the_cards_order() {
    for (binding, path) in [
        (A2aBinding::JsonRpc, "/rpc"),
        (A2aBinding::HttpJson, "/rest"),
    ] {
        let fixture = A2aFixture::start(Script::over(binding));
        let client = connect(&fixture).await;

        let requests = fixture.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].target, "/.well-known/agent-card.json");
        assert_eq!(client.card().name, "Nous");
        // The card lists a gRPC interface first and both bindings after it;
        // whichever of those comes first is chosen.
        assert_eq!(client.binding(), binding);
        assert_eq!(
            client.endpoint().as_str(),
            format!("{}{path}", fixture.base())
        );
    }
}

#[tokio::test]
async fn a_card_without_a_spoken_binding_at_major_version_1_is_refused() {
    let mut card = standard_card("http://127.0.0.1:1", A2aBinding::JsonRpc);
    card["supportedInterfaces"] = json!([
        {"url": "http://127.0.0.1:1/grpc", "protocolBinding": "GRPC", "protocolVersion": "1.0"},
        {"url": "http://127.0.0.1:1/old", "protocolBinding": "JSONRPC", "protocolVersion": "0.3"},
        {"url": "http://127.0.0.1:1/new", "protocolBinding": "HTTP+JSON", "protocolVersion": "2.0"},
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
fn any_protocol_1_x_is_spoken_to_as_1_0() {
    for accepted in ["1.0", "1.0.3", "1", " 1.0 ", "1.1"] {
        assert!(speaks_version(accepted), "{accepted}");
    }
    for refused in ["0.3", "2.0", "", "one", "1.x"] {
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
    for binding in BINDINGS {
        let fixture = A2aFixture::start(Script::over(binding));
        let client = connect(&fixture).await;
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
        assert_eq!(fixture.operations(), ["GetTask", "SendMessage"]);
    }
}

#[tokio::test]
async fn send_message_puts_the_configuration_and_message_on_the_wire() {
    for binding in BINDINGS {
        let fixture = A2aFixture::start(Script::over(binding));
        let client = connect(&fixture).await;

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
}

#[tokio::test]
async fn http_json_gives_every_method_its_verb_and_path() {
    let fixture = A2aFixture::start(Script::over(A2aBinding::HttpJson));
    let client = connect(&fixture).await;

    client
        .send_message(user_message("hello"), immediately())
        .await
        .expect("send");
    client.get_task("t1", Some(0)).await.expect("get");
    client.cancel_task("t1").await.expect("cancel");

    assert_eq!(
        targets(&fixture),
        [
            "POST /rest/message:send",
            "GET /rest/tasks/t1?historyLength=0",
            "POST /rest/tasks/t1:cancel",
        ]
    );
}

#[tokio::test]
async fn an_id_the_agent_chose_cannot_add_path_segments() {
    let fixture = A2aFixture::start(Script {
        get: vec![RpcAnswer::Result(task_json(
            "../a/b?c",
            "c1",
            "TASK_STATE_WORKING",
        ))],
        ..Script::over(A2aBinding::HttpJson)
    });
    let client = connect(&fixture).await;

    client.get_task("../a/b?c", None).await.expect("get");
    assert_eq!(targets(&fixture), ["GET /rest/tasks/..%2Fa%2Fb%3Fc"]);
}

#[tokio::test]
async fn the_interfaces_tenant_is_sent_on_every_request() {
    for binding in BINDINGS {
        let fixture = A2aFixture::start(Script {
            tenant: Some("acme"),
            ..Script::over(binding)
        });
        let client = connect(&fixture).await;
        assert_eq!(client.tenant(), Some("acme"));

        client.get_task("t1", None).await.expect("get");
        client
            .send_message(user_message("hello"), immediately())
            .await
            .expect("send");

        match binding {
            A2aBinding::HttpJson => assert_eq!(
                targets(&fixture),
                ["GET /rest/acme/tasks/t1", "POST /rest/acme/message:send"]
            ),
            _ => {
                assert_eq!(fixture.params_of("GetTask")[0]["tenant"], json!("acme"));
                assert_eq!(fixture.params_of("SendMessage")[0]["tenant"], json!("acme"));
            }
        }
    }
}

#[tokio::test]
async fn a_plaintext_endpoint_named_by_the_card_is_refused_when_credentials_are_configured() {
    let mut card = standard_card("http://127.0.0.1:1", A2aBinding::JsonRpc);
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
async fn a2a_errors_map_to_typed_codes_without_server_text() {
    for binding in BINDINGS {
        let fixture = A2aFixture::start(Script {
            get: vec![RpcAnswer::Error(-32001)],
            cancel: RpcAnswer::Error(-32002),
            ..Script::over(binding)
        });
        let client = connect(&fixture).await;

        let error = client.get_task("t9", None).await.expect_err("not found");
        assert!(
            matches!(
                error,
                A2aError::Rpc {
                    method: "GetTask",
                    error: A2aRpcError::TaskNotFound
                }
            ),
            "{binding}: {error}"
        );
        let text = error.to_string();
        assert!(text.contains("TaskNotFoundError (-32001)"), "{text}");
        assert!(!text.contains("server text"), "{text}");

        let error = client.cancel_task("t1").await.expect_err("not cancelable");
        assert!(
            matches!(
                error,
                A2aError::Rpc {
                    error: A2aRpcError::TaskNotCancelable,
                    ..
                }
            ),
            "{binding}: {error}"
        );
    }
}

#[tokio::test]
async fn an_http_json_failure_without_an_a2a_reason_is_reported_by_its_status() {
    let fixture = A2aFixture::start(Script {
        get: vec![RpcAnswer::Http(
            404,
            json!({"error": {"code": 404, "message": "no route", "details": [{
                "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                "reason": "TASK_NOT_FOUND",
                "domain": "gateway.example.com",
            }]}})
            .to_string(),
        )],
        ..Script::over(A2aBinding::HttpJson)
    });
    let client = connect(&fixture).await;

    let error = client.get_task("t1", None).await.expect_err("not found");
    assert!(
        matches!(error, A2aError::HttpStatus { status, .. } if status == 404),
        "a reason outside the A2A domain is not the agent's: {error}"
    );
}

#[tokio::test]
async fn an_oversized_reply_is_bounded() {
    for binding in BINDINGS {
        let fixture = A2aFixture::start(Script {
            get: vec![RpcAnswer::Http(200, "x".repeat(64 * 1024))],
            ..Script::over(binding)
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
}

#[tokio::test]
async fn http_failures_never_show_credentials_or_server_bodies() {
    for binding in BINDINGS {
        let fixture = A2aFixture::start(Script {
            get: vec![RpcAnswer::Http(401, format!("bad token {SECRET}"))],
            cancel: RpcAnswer::Http(302, String::new()),
            ..Script::over(binding)
        });
        let client = connect(&fixture).await;

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
}

#[tokio::test]
async fn a_send_that_may_have_been_accepted_is_indeterminate() {
    for binding in BINDINGS {
        for (answer, indeterminate) in [
            (RpcAnswer::Http(502, "bad gateway".to_string()), true),
            (RpcAnswer::Http(200, "not json".to_string()), true),
            (RpcAnswer::Http(400, String::new()), false),
            (RpcAnswer::Error(-32602), false),
        ] {
            let fixture = A2aFixture::start(Script {
                send: answer.clone(),
                ..Script::over(binding)
            });
            let client = connect(&fixture).await;
            let error = client
                .send_message(user_message("hello"), immediately())
                .await
                .expect_err("send fails");
            assert_eq!(
                matches!(error, A2aError::SendIndeterminate),
                indeterminate,
                "{binding}: {answer:?} -> {error}"
            );
        }
    }
}

#[tokio::test]
async fn an_answer_about_a_different_task_is_refused() {
    for binding in BINDINGS {
        let fixture = A2aFixture::start(Script {
            get: vec![RpcAnswer::Result(task_json(
                "other",
                "c1",
                "TASK_STATE_WORKING",
            ))],
            ..Script::over(binding)
        });
        let client = connect(&fixture).await;

        let error = client.get_task("t1", None).await.expect_err("mismatch");
        assert!(matches!(
            error,
            A2aError::MismatchedTask { method: "GetTask" }
        ));
    }
}

#[tokio::test]
async fn get_task_decodes_state_and_artifacts() {
    for binding in BINDINGS {
        let mut task = task_json("t1", "c1", "TASK_STATE_COMPLETED");
        task["artifacts"] = json!([{"artifactId": "a1", "parts": [{"text": "the answer"}]}]);
        let fixture = A2aFixture::start(Script {
            get: vec![RpcAnswer::Result(task)],
            ..Script::over(binding)
        });
        let client = connect(&fixture).await;

        let task = client.get_task("t1", Some(0)).await.expect("get");
        assert_eq!(task.status.state, TaskState::Completed);
        assert_eq!(task.context_id, "c1");
        let artifacts = task.artifacts.expect("artifacts");
        assert_eq!(artifacts[0].parts[0].as_text(), Some("the answer"));
        let history_length = &fixture.params_of("GetTask")[0]["historyLength"];
        match binding {
            A2aBinding::HttpJson => assert_eq!(*history_length, json!("0")),
            _ => assert_eq!(*history_length, json!(0)),
        }
    }
}
