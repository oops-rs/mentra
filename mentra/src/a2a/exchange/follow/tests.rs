//! The exchange against an agent that streams: settling by its stream, and
//! falling back to polling — never to resending — when the stream fails.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use a2a_lf::TaskState;
use serde_json::{Value, json};

use crate::a2a::exchange::{A2aExchange, A2aExchangeError, A2aStillWorking, exchange};
use crate::a2a::testing::{A2aFixture, RpcAnswer, Script, StreamEnd, status_event, task_json};
use crate::a2a::{A2aAgentConfig, A2aBinding, A2aClient, A2aLimits, A2aRequest, remote_output};
use crate::runtime::{CancellationToken, RunOptions};

const BINDINGS: [A2aBinding; 2] = [A2aBinding::JsonRpc, A2aBinding::HttpJson];

fn limits() -> A2aLimits {
    A2aLimits {
        poll_interval: Duration::from_millis(10),
        max_wait: Duration::from_secs(5),
        stream_idle_timeout: Duration::from_secs(5),
        ..A2aLimits::default()
    }
}

/// An agent that streams, whose `SendMessage` answers with a working task.
fn streaming(binding: A2aBinding, subscribe: RpcAnswer) -> Script {
    Script {
        streaming: true,
        send: RpcAnswer::Result(json!({"task": task_json("t1", "c1", "TASK_STATE_WORKING")})),
        subscribe: vec![subscribe],
        ..Script::over(binding)
    }
}

async fn client_with(fixture: &A2aFixture, limits: A2aLimits) -> Arc<A2aClient> {
    let config = A2aAgentConfig::new("nous", fixture.base()).with_limits(limits);
    Arc::new(A2aClient::connect(&config).await.expect("connect"))
}

fn artifact(text: &str, append: bool) -> Value {
    json!({"artifactUpdate": {"taskId": "t1", "contextId": "c1", "append": append, "artifact": {
        "artifactId": "a1", "parts": [{"text": text}],
    }}})
}

fn completed_with(text: &str) -> RpcAnswer {
    let mut task = task_json("t1", "c1", "TASK_STATE_COMPLETED");
    task["artifacts"] = json!([{"artifactId": "a1", "parts": [{"text": text}]}]);
    RpcAnswer::Result(task)
}

/// Runs one exchange, returning its outcome and the progress it reported.
async fn run(
    client: &Arc<A2aClient>,
    options: &RunOptions,
) -> (Result<A2aExchange, A2aExchangeError>, Vec<String>) {
    let heard = Mutex::new(Vec::new());
    let progress = |line: String| heard.lock().expect("progress").push(line);
    let outcome = exchange(client, A2aRequest::send("ask"), options, &progress).await;
    (outcome, heard.into_inner().expect("progress"))
}

fn sends(fixture: &A2aFixture) -> usize {
    fixture
        .operations()
        .iter()
        .filter(|operation| operation.contains("SendMessage"))
        .count()
}

#[tokio::test]
async fn a_streaming_agents_task_settles_by_its_stream() {
    for binding in BINDINGS {
        let events = vec![
            json!({"task": task_json("t1", "c1", "TASK_STATE_WORKING")}),
            status_event("TASK_STATE_WORKING", Some("reading\nthe code")),
            artifact("4", false),
            artifact("2", true),
            status_event("TASK_STATE_COMPLETED", None),
        ];
        let fixture = A2aFixture::start(Script {
            // Polling would fail; the stream is all there is to read.
            get: vec![RpcAnswer::Error(-32001)],
            ..streaming(binding, RpcAnswer::Events(events, StreamEnd::Hang))
        });
        let client = client_with(&fixture, limits()).await;

        let (outcome, heard) = run(&client, &RunOptions::default()).await;
        let Ok(A2aExchange::Settled(task)) = outcome else {
            panic!("{binding}: expected a settled task, got {outcome:?}");
        };
        assert_eq!(task.status.state, TaskState::Completed);
        // An appended chunk joins the artifact's parts, as the SDKs merge it.
        let artifacts = task.artifacts.as_deref().unwrap_or_default();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].parts.len(), 2);
        assert!(remote_output("nous", &task, 1024).contains("\n4\n2\n"));
        assert_eq!(fixture.operations(), ["SendMessage", "SubscribeToTask"]);
        assert_eq!(
            heard,
            [
                "remote task t1 is working: reading the code",
                "remote task t1 is completed",
            ]
        );
    }
}

#[tokio::test]
async fn a_stream_dropped_mid_task_falls_back_to_polling_without_resending() {
    for binding in BINDINGS {
        let events = vec![
            json!({"task": task_json("t1", "c1", "TASK_STATE_WORKING")}),
            status_event("TASK_STATE_WORKING", Some("halfway")),
        ];
        let fixture = A2aFixture::start(Script {
            get: vec![completed_with("42")],
            ..streaming(binding, RpcAnswer::Events(events, StreamEnd::Close))
        });
        let client = client_with(&fixture, limits()).await;

        let (outcome, heard) = run(&client, &RunOptions::default()).await;
        let Ok(A2aExchange::Settled(task)) = outcome else {
            panic!("{binding}: expected a settled task, got {outcome:?}");
        };
        assert!(remote_output("nous", &task, 1024).contains("\n42\n"));
        assert_eq!(
            fixture.operations(),
            ["SendMessage", "SubscribeToTask", "GetTask"]
        );
        assert_eq!(sends(&fixture), 1, "the message is never sent again");
        assert_eq!(
            heard,
            [
                "remote task t1 is working: halfway",
                "remote task t1 is completed",
            ]
        );
    }
}

#[tokio::test]
async fn a_refused_or_silent_stream_falls_back_to_polling() {
    let silent = RpcAnswer::Events(
        vec![json!({"task": task_json("t1", "c1", "TASK_STATE_WORKING")})],
        StreamEnd::Hang,
    );
    for (subscribe, why) in [
        (RpcAnswer::Error(-32004), "refused"),
        (RpcAnswer::Http(503, String::new()), "unavailable"),
        (
            RpcAnswer::RawEvents("data: {not json\n\n".into(), StreamEnd::Hang),
            "malformed",
        ),
        (silent, "silent"),
    ] {
        let fixture = A2aFixture::start(Script {
            get: vec![completed_with("42")],
            ..streaming(A2aBinding::HttpJson, subscribe)
        });
        let client = client_with(
            &fixture,
            A2aLimits {
                stream_idle_timeout: Duration::from_millis(100),
                ..limits()
            },
        )
        .await;

        let (outcome, _) = run(&client, &RunOptions::default()).await;
        assert!(
            matches!(outcome, Ok(A2aExchange::Settled(_))),
            "{why}: {outcome:?}"
        );
        assert_eq!(sends(&fixture), 1, "{why}");
        assert_eq!(
            fixture.operations().last().map(String::as_str),
            Some("GetTask")
        );
    }
}

#[tokio::test]
async fn a_followed_task_still_working_at_the_bound_is_handed_back() {
    let bound = Duration::from_millis(200);
    let fixture = A2aFixture::start(streaming(
        A2aBinding::JsonRpc,
        RpcAnswer::Events(
            vec![json!({"task": task_json("t1", "c1", "TASK_STATE_WORKING")})],
            StreamEnd::Hang,
        ),
    ));
    let client = client_with(
        &fixture,
        A2aLimits {
            max_wait: bound,
            ..limits()
        },
    )
    .await;

    let (outcome, _) = run(&client, &RunOptions::default()).await;
    let Ok(A2aExchange::StillWorking { task, reason }) = outcome else {
        panic!("expected a working task, got {outcome:?}");
    };
    assert_eq!(task.id, "t1");
    assert_eq!(reason, A2aStillWorking::WaitBound(bound));
    assert_eq!(fixture.operations(), ["SendMessage", "SubscribeToTask"]);
}

#[tokio::test]
async fn a_run_cancelled_while_following_cancels_the_task() {
    let fixture = A2aFixture::start(streaming(
        A2aBinding::HttpJson,
        RpcAnswer::Events(
            vec![json!({"task": task_json("t1", "c1", "TASK_STATE_WORKING")})],
            StreamEnd::Hang,
        ),
    ));
    let client = client_with(&fixture, limits()).await;
    let token = CancellationToken::default();
    let options = RunOptions {
        cancellation: Some(token.clone()),
        ..RunOptions::default()
    };

    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        token.cancel();
    });
    let (outcome, _) = run(&client, &options).await;
    canceller.await.expect("canceller");

    let Err(A2aExchangeError::Abandoned { task_id, canceled }) = outcome else {
        panic!("expected an abandoned task, got {outcome:?}");
    };
    assert_eq!(task_id, "t1");
    assert!(canceled.is_ok());
    assert_eq!(
        fixture.operations(),
        ["SendMessage", "SubscribeToTask", "CancelTask"]
    );
}

#[tokio::test]
async fn a_run_stopping_while_following_hands_the_task_back() {
    let fixture = A2aFixture::start(streaming(
        A2aBinding::JsonRpc,
        RpcAnswer::Events(
            vec![json!({"task": task_json("t1", "c1", "TASK_STATE_WORKING")})],
            StreamEnd::Hang,
        ),
    ));
    let client = client_with(&fixture, limits()).await;
    let stop = CancellationToken::default();
    let options = RunOptions {
        stop: Some(stop.clone()),
        ..RunOptions::default()
    };

    let stopper = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        stop.cancel();
    });
    let (outcome, _) = run(&client, &options).await;
    stopper.await.expect("stopper");

    let Ok(A2aExchange::StillWorking { reason, .. }) = outcome else {
        panic!("expected a working task, got {outcome:?}");
    };
    assert_eq!(reason, A2aStillWorking::RunStopping);
    assert!(!fixture.operations().contains(&"CancelTask".to_string()));
}

#[tokio::test]
async fn an_agent_that_does_not_stream_is_never_subscribed_to() {
    let fixture = A2aFixture::start(Script {
        send: RpcAnswer::Result(json!({"task": task_json("t1", "c1", "TASK_STATE_WORKING")})),
        get: vec![completed_with("42")],
        ..Script::default()
    });
    let client = client_with(&fixture, limits()).await;

    let (outcome, _) = run(&client, &RunOptions::default()).await;
    assert!(
        matches!(outcome, Ok(A2aExchange::Settled(_))),
        "{outcome:?}"
    );
    assert_eq!(fixture.operations(), ["SendMessage", "GetTask"]);
}

#[tokio::test]
async fn an_exchange_dropped_while_following_cancels_the_task() {
    let fixture = A2aFixture::start(streaming(
        A2aBinding::JsonRpc,
        RpcAnswer::Events(
            vec![json!({"task": task_json("t1", "c1", "TASK_STATE_WORKING")})],
            StreamEnd::Hang,
        ),
    ));
    let client = client_with(&fixture, limits()).await;

    let dropped = tokio::time::timeout(
        Duration::from_millis(150),
        run(&client, &RunOptions::default()),
    )
    .await;
    assert!(dropped.is_err(), "the exchange should still be following");

    for _ in 0..50 {
        if fixture.operations().contains(&"CancelTask".to_string()) {
            assert_eq!(sends(&fixture), 1);
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "no CancelTask after the exchange was dropped: {:?}",
        fixture.operations()
    );
}
