use a2a_lf::AuthenticationInfo;
use serde_json::{Value, json};

use super::*;
use crate::a2a::testing::{A2aFixture, RpcAnswer, Script};
use crate::a2a::{A2aAgentConfig, A2aBinding};

const BINDINGS: [A2aBinding; 2] = [A2aBinding::JsonRpc, A2aBinding::HttpJson];
const WEBHOOK_SECRET: &str = "webhook-credential";

fn stored(task_id: &str) -> Value {
    json!({"id": "cfg1", "taskId": task_id, "url": "https://hooks.example.com/a2a"})
}

fn script(binding: A2aBinding, answers: Vec<(&'static str, RpcAnswer)>) -> Script {
    Script {
        tenant: Some("acme"),
        other: answers,
        ..Script::over(binding)
    }
}

async fn connect(fixture: &A2aFixture) -> A2aClient {
    A2aClient::connect(&A2aAgentConfig::new("nous", fixture.base()))
        .await
        .expect("connect")
}

/// The verb and target of every request after the card fetch.
fn targets(fixture: &A2aFixture) -> Vec<String> {
    fixture.requests()[1..]
        .iter()
        .map(|request| format!("{} {}", request.method, request.target))
        .collect()
}

#[tokio::test]
async fn every_push_config_method_reaches_its_operation() {
    for binding in BINDINGS {
        let fixture = A2aFixture::start(script(
            binding,
            vec![
                (
                    "CreateTaskPushNotificationConfig",
                    RpcAnswer::Result(stored("t1")),
                ),
                (
                    "GetTaskPushNotificationConfig",
                    RpcAnswer::Result(stored("t1")),
                ),
                (
                    "ListTaskPushNotificationConfigs",
                    RpcAnswer::Result(json!({"configs": [stored("t1")]})),
                ),
                // Empty in both bindings: an empty body, or a null result.
                (
                    "DeleteTaskPushNotificationConfig",
                    match binding {
                        A2aBinding::HttpJson => RpcAnswer::Http(200, String::new()),
                        _ => RpcAnswer::Result(Value::Null),
                    },
                ),
            ],
        ));
        let client = connect(&fixture).await;

        let created = client
            .create_push_config(TaskPushNotificationConfig {
                url: "https://hooks.example.com/a2a".to_string(),
                id: None,
                task_id: "t1".to_string(),
                token: Some("per-task-token".to_string()),
                authentication: Some(AuthenticationInfo {
                    scheme: "Bearer".to_string(),
                    credentials: Some(WEBHOOK_SECRET.to_string()),
                }),
                tenant: None,
            })
            .await
            .expect("create");
        assert_eq!(created.id.as_deref(), Some("cfg1"));
        client.get_push_config("t1", "cfg1").await.expect("get");
        let listed = client
            .list_push_configs(ListTaskPushNotificationConfigsRequest {
                task_id: "t1".to_string(),
                page_size: Some(5),
                page_token: None,
                tenant: None,
            })
            .await
            .expect("list");
        assert_eq!(listed.configs.len(), 1);
        client
            .delete_push_config("t1", "cfg1")
            .await
            .expect("delete");

        assert_eq!(
            fixture.operations(),
            [
                "CreateTaskPushNotificationConfig",
                "GetTaskPushNotificationConfig",
                "ListTaskPushNotificationConfigs",
                "DeleteTaskPushNotificationConfig",
            ]
        );
        let sent = &fixture.params_of("CreateTaskPushNotificationConfig")[0];
        assert_eq!(sent["url"], json!("https://hooks.example.com/a2a"));
        assert_eq!(sent["authentication"]["credentials"], json!(WEBHOOK_SECRET));
        match binding {
            A2aBinding::HttpJson => assert_eq!(
                targets(&fixture),
                [
                    "POST /rest/acme/tasks/t1/pushNotificationConfigs",
                    "GET /rest/acme/tasks/t1/pushNotificationConfigs/cfg1",
                    "GET /rest/acme/tasks/t1/pushNotificationConfigs?pageSize=5",
                    "DELETE /rest/acme/tasks/t1/pushNotificationConfigs/cfg1",
                ]
            ),
            _ => assert_eq!(sent["tenant"], json!("acme")),
        }
    }
}

#[tokio::test]
async fn a_config_for_another_task_is_refused() {
    for binding in BINDINGS {
        let fixture = A2aFixture::start(script(
            binding,
            vec![
                (
                    "GetTaskPushNotificationConfig",
                    RpcAnswer::Result(stored("t2")),
                ),
                (
                    "ListTaskPushNotificationConfigs",
                    RpcAnswer::Result(json!({"configs": [stored("t1"), stored("t2")]})),
                ),
            ],
        ));
        let client = connect(&fixture).await;

        let error = client
            .get_push_config("t1", "cfg1")
            .await
            .expect_err("other task");
        assert!(matches!(error, A2aError::MismatchedTask { .. }), "{error}");
        let error = client
            .list_push_configs(ListTaskPushNotificationConfigsRequest {
                task_id: "t1".to_string(),
                page_size: None,
                page_token: None,
                tenant: None,
            })
            .await
            .expect_err("other task");
        assert!(matches!(error, A2aError::MismatchedTask { .. }), "{error}");
    }
}

#[tokio::test]
async fn an_agent_without_push_support_is_reported_by_code() {
    for binding in BINDINGS {
        let fixture = A2aFixture::start(script(
            binding,
            vec![("GetTaskPushNotificationConfig", RpcAnswer::Error(-32003))],
        ));
        let client = connect(&fixture).await;

        let error = client
            .get_push_config("t1", "cfg1")
            .await
            .expect_err("unsupported");
        assert!(
            matches!(
                error,
                A2aError::Rpc {
                    error: crate::a2a::A2aRpcError::PushNotificationNotSupported,
                    ..
                }
            ),
            "{binding}: {error}"
        );
    }
}
