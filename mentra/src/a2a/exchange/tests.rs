use serde_json::{Value, json};

use super::*;
use crate::a2a::testing::{A2aFixture, RpcAnswer, Script, task_json};
use crate::a2a::{A2aAgentConfig, A2aBinding, A2aLimits, remote_output, remote_reply};
use crate::runtime::CancellationToken;

fn fast_limits() -> A2aLimits {
    A2aLimits {
        poll_interval: Duration::from_millis(10),
        max_wait: Duration::from_secs(5),
        ..A2aLimits::default()
    }
}

async fn client_with(fixture: &A2aFixture, limits: A2aLimits) -> Arc<A2aClient> {
    let config = A2aAgentConfig::new("nous", fixture.base()).with_limits(limits);
    Arc::new(A2aClient::connect(&config).await.expect("connect"))
}

fn send(text: &str) -> A2aRequest {
    A2aRequest::send(text)
}

fn working() -> RpcAnswer {
    RpcAnswer::Result(task_json("t1", "c1", "TASK_STATE_WORKING"))
}

fn with_artifact(mut task: Value, text: &str) -> Value {
    task["artifacts"] = json!([{"artifactId": "a1", "parts": [{"text": text}]}]);
    task
}

#[tokio::test]
async fn a_direct_reply_comes_back_as_the_message() {
    let fixture = A2aFixture::start(Script {
        send: RpcAnswer::Result(json!({"message": {
            "messageId": "m1",
            "contextId": "c9",
            "role": "ROLE_AGENT",
            "parts": [{"text": "hello back"}],
        }})),
        ..Script::default()
    });
    let client = client_with(&fixture, fast_limits()).await;

    let outcome = exchange(&client, send("hello"), &RunOptions::default(), &|_| {})
        .await
        .expect("reply");
    let A2aExchange::Reply(message) = outcome else {
        panic!("expected a reply, got {outcome:?}");
    };
    assert_eq!(message.context_id.as_deref(), Some("c9"));
    assert_eq!(
        remote_reply("nous", &message, 1024),
        "--- begin output from remote agent `nous` ---\nhello back\n\
         --- end output from remote agent `nous` ---"
    );
    assert_eq!(fixture.operations(), ["SendMessage"]);
}

#[tokio::test]
async fn a_task_is_polled_until_it_settles() {
    let completed = with_artifact(task_json("t1", "c1", "TASK_STATE_COMPLETED"), "42");
    let fixture = A2aFixture::start(Script {
        send: RpcAnswer::Result(json!({"task": task_json("t1", "c1", "TASK_STATE_SUBMITTED")})),
        get: vec![working(), RpcAnswer::Result(completed)],
        ..Script::default()
    });
    let client = client_with(&fixture, fast_limits()).await;

    let outcome = exchange(&client, send("ask"), &RunOptions::default(), &|_| {})
        .await
        .expect("settled");
    let A2aExchange::Settled(task) = outcome else {
        panic!("expected a settled task, got {outcome:?}");
    };
    assert_eq!(task.status.state, TaskState::Completed);
    assert_eq!(task.context_id, "c1");
    assert!(remote_output("nous", &task, 1024).contains("\n42\n"));

    let sent = &fixture.params_of("SendMessage")[0];
    assert_eq!(sent["configuration"]["returnImmediately"], json!(true));
    assert_eq!(sent["configuration"]["historyLength"], json!(0));
}

#[tokio::test]
async fn submit_returns_the_task_before_anything_waits_on_it() {
    // The two halves exist so a host can record the task between them: a
    // process that dies while settling must find the task, not resend it.
    let completed = with_artifact(task_json("t1", "c1", "TASK_STATE_COMPLETED"), "42");
    let fixture = A2aFixture::start(Script {
        send: RpcAnswer::Result(json!({"task": task_json("t1", "c1", "TASK_STATE_SUBMITTED")})),
        get: vec![RpcAnswer::Result(completed)],
        ..Script::default()
    });
    let client = client_with(&fixture, fast_limits()).await;

    let A2aSubmitted::Task(task) = submit(&client, send("ask")).await.expect("submitted") else {
        panic!("expected a task");
    };
    assert_eq!(task.id, "t1");
    assert_eq!(
        fixture.operations(),
        ["SendMessage"],
        "nothing has polled yet"
    );

    let outcome = settle(&client, task, &RunOptions::default(), &|_| {})
        .await
        .expect("settled");
    assert!(matches!(outcome, A2aExchange::Settled(_)), "{outcome:?}");
    assert_eq!(fixture.operations(), ["SendMessage", "GetTask"]);
}

#[tokio::test]
async fn a_resume_waits_without_sending() {
    let fixture = A2aFixture::start(Script {
        get: vec![RpcAnswer::Result(with_artifact(
            task_json("t1", "c1", "TASK_STATE_COMPLETED"),
            "done",
        ))],
        ..Script::default()
    });
    let client = client_with(&fixture, fast_limits()).await;

    let request = A2aRequest::resume("t1");
    let outcome = exchange(&client, request, &RunOptions::default(), &|_| {})
        .await
        .expect("settled");
    assert!(matches!(outcome, A2aExchange::Settled(_)));
    assert_eq!(fixture.operations(), ["GetTask"]);
}

#[tokio::test]
async fn a_task_still_working_at_the_bound_is_handed_back() {
    let fixture = A2aFixture::start(Script {
        send: RpcAnswer::Result(json!({"task": task_json("t1", "c1", "TASK_STATE_WORKING")})),
        get: vec![working()],
        ..Script::default()
    });
    let bound = Duration::from_millis(50);
    let client = client_with(
        &fixture,
        A2aLimits {
            max_wait: bound,
            ..fast_limits()
        },
    )
    .await;

    let outcome = exchange(&client, send("long job"), &RunOptions::default(), &|_| {})
        .await
        .expect("handed back");
    let A2aExchange::StillWorking { task, reason } = outcome else {
        panic!("expected a working task, got {outcome:?}");
    };
    assert_eq!(task.id, "t1");
    assert_eq!(reason, A2aStillWorking::WaitBound(bound));
    assert!(
        !fixture.operations().contains(&"CancelTask".to_string()),
        "a task handed back is left running"
    );
}

#[tokio::test]
async fn a_failed_poll_names_the_task() {
    let fixture = A2aFixture::start(Script {
        send: RpcAnswer::Result(json!({"task": task_json("t1", "c1", "TASK_STATE_WORKING")})),
        get: vec![RpcAnswer::Error(-32001)],
        ..Script::default()
    });
    let client = client_with(&fixture, fast_limits()).await;

    let error = exchange(&client, send("go"), &RunOptions::default(), &|_| {})
        .await
        .expect_err("poll failed");
    let A2aExchangeError::Poll { task_id, .. } = &error else {
        panic!("expected a poll failure, got {error:?}");
    };
    assert_eq!(task_id, "t1");
    assert!(
        error.to_string().contains("checking on task t1 failed"),
        "{error}"
    );
}

#[tokio::test]
async fn a_failed_send_starts_nothing() {
    let fixture = A2aFixture::start(Script {
        send: RpcAnswer::Error(-32603),
        ..Script::default()
    });
    let client = client_with(&fixture, fast_limits()).await;

    let error = exchange(&client, send("go"), &RunOptions::default(), &|_| {})
        .await
        .expect_err("send failed");
    assert!(matches!(error, A2aExchangeError::Send(_)), "{error:?}");
    assert_eq!(fixture.operations(), ["SendMessage"]);
}

#[tokio::test]
async fn a_cancelled_run_abandons_and_cancels_the_task() {
    let fixture = A2aFixture::start(Script {
        send: RpcAnswer::Result(json!({"task": task_json("t1", "c1", "TASK_STATE_WORKING")})),
        get: vec![working()],
        ..Script::default()
    });
    let client = client_with(&fixture, fast_limits()).await;
    let token = CancellationToken::default();
    let options = RunOptions {
        cancellation: Some(token.clone()),
        ..RunOptions::default()
    };

    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();
    });
    let error = exchange(&client, send("long job"), &options, &|_| {})
        .await
        .expect_err("cancelled");
    canceller.await.expect("canceller");

    let A2aExchangeError::Abandoned { task_id, canceled } = &error else {
        panic!("expected an abandoned task, got {error:?}");
    };
    assert_eq!(task_id, "t1");
    assert!(canceled.is_ok());
    assert!(error.to_string().contains("asked to cancel"), "{error}");
    assert_eq!(fixture.params_of("CancelTask")[0]["id"], json!("t1"));
}

#[test]
fn settled_means_terminal_or_waiting_on_the_caller() {
    assert!(is_settled(&TaskState::Completed));
    assert!(is_settled(&TaskState::InputRequired));
    assert!(is_settled(&TaskState::AuthRequired));
    assert!(!is_settled(&TaskState::Working));
    assert!(!is_settled(&TaskState::Submitted));
}

#[tokio::test]
async fn a_message_carries_its_metadata_and_activates_its_extensions() {
    const HOPS: &str = "https://example.com/a2a/hops/v1";
    const TRACE: &str = "https://example.com/a2a/trace/v1";
    for binding in [A2aBinding::JsonRpc, A2aBinding::HttpJson] {
        let fixture = A2aFixture::start(Script::over(binding));
        let client = client_with(&fixture, fast_limits()).await;

        let request = A2aSend::new("ask")
            .with_context_id("c1")
            .with_metadata(HOPS, json!({"hop": 2}))
            .with_extension(HOPS)
            .with_extension(TRACE)
            .with_extension(HOPS);
        exchange(&client, request.into(), &RunOptions::default(), &|_| {})
            .await
            .expect("settled");

        let sent = &fixture.params_of("SendMessage")[0]["message"];
        assert_eq!(sent["metadata"][HOPS], json!({"hop": 2}));
        assert_eq!(sent["extensions"], json!([HOPS, TRACE]));
        assert_eq!(sent["contextId"], json!("c1"));
        let requests = fixture.requests();
        assert_eq!(
            requests[1].header("a2a-extensions"),
            Some(format!("{HOPS},{TRACE}").as_str()),
            "{binding}"
        );
        // Only the message activates them; reading the task does not.
        assert!(
            requests[2..]
                .iter()
                .all(|request| request.header("a2a-extensions").is_none())
        );
    }
}

#[tokio::test]
async fn a_message_without_extensions_activates_none() {
    let fixture = A2aFixture::start(Script::default());
    let client = client_with(&fixture, fast_limits()).await;
    exchange(&client, send("ask"), &RunOptions::default(), &|_| {})
        .await
        .expect("settled");

    let sent = &fixture.params_of("SendMessage")[0]["message"];
    assert_eq!(sent.get("metadata"), None);
    assert_eq!(sent.get("extensions"), None);
    assert_eq!(fixture.requests()[1].header("a2a-extensions"), None);
}

#[tokio::test]
async fn an_extension_uri_that_would_change_the_list_is_refused_before_sending() {
    let fixture = A2aFixture::start(Script::default());
    let client = client_with(&fixture, fast_limits()).await;

    for uri in [
        "",
        "https://a.example/x,https://b.example/y",
        "https://a.example/ x",
    ] {
        let request = A2aSend::new("ask").with_extension(uri);
        let error = submit(&client, request.into()).await.expect_err("refused");
        assert!(
            matches!(error, A2aExchangeError::Send(A2aError::InvalidExtensionUri)),
            "{uri:?}: {error}"
        );
    }
    assert!(fixture.operations().is_empty(), "nothing was sent");
}
