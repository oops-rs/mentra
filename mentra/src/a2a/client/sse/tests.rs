use std::time::Duration;

use a2a_lf::{Message, Part, Role, SendMessageConfiguration, TaskState};
use futures_util::StreamExt;
use serde_json::json;

use super::*;
use crate::a2a::testing::{
    A2aFixture, RpcAnswer, Script, StreamEnd, rest_error, status_event, task_json,
};
use crate::a2a::{A2aAgentConfig, A2aBinding, A2aClient};

const BINDINGS: [A2aBinding; 2] = [A2aBinding::JsonRpc, A2aBinding::HttpJson];

fn parse(chunks: &[&[u8]]) -> Vec<String> {
    let mut parser = SseParser::new(1024);
    let mut events = Vec::new();
    for chunk in chunks {
        events.extend(parser.push(chunk).expect("within the limit"));
    }
    events.extend(parser.finish());
    events
        .into_iter()
        .map(|data| String::from_utf8(data).expect("utf-8"))
        .collect()
}

#[test]
fn events_end_at_a_blank_line_whatever_the_line_ending() {
    assert_eq!(
        parse(&[b"data: a\n\ndata: b\r\n\r\ndata: c\r\rdata: d\n\n"]),
        ["a", "b", "c", "d"]
    );
}

#[test]
fn a_line_ending_split_across_chunks_ends_one_line() {
    // CR then LF in the next chunk is one line ending, not a blank line.
    assert_eq!(
        parse(&[b"data: a\r", b"\ndata: b\r", b"\n\r", b"\n"]),
        ["a\nb"]
    );
}

#[test]
fn data_lines_join_and_everything_else_is_ignored() {
    assert_eq!(
        parse(&[b": keep-alive\n\nid: 7\nevent: update\ndata:{\"a\":\ndata:  1}\nretry: 5\n\n"]),
        ["{\"a\":\n 1}"]
    );
}

#[test]
fn an_event_the_agent_never_ended_is_still_read() {
    assert_eq!(parse(&[b"data: whole\n\ndata: cut"]), ["whole", "cut"]);
    assert!(parse(&[b": only a comment\n"]).is_empty());
}

#[test]
fn an_event_larger_than_the_limit_is_refused_before_it_is_buffered() {
    let mut parser = SseParser::new(16);
    let error = parser
        .push(b"data: 0123456789abcdef")
        .expect_err("too large");
    assert!(matches!(error, A2aError::EventTooLarge { limit: 16 }));

    // Lines of one event count together.
    let mut parser = SseParser::new(16);
    parser.push(b"data: 01234\n").expect("fits");
    let error = parser.push(b"data: 567890").expect_err("too large");
    assert!(matches!(error, A2aError::EventTooLarge { limit: 16 }));
}

fn limits() -> A2aLimits {
    A2aLimits {
        stream_idle_timeout: Duration::from_millis(200),
        ..A2aLimits::default()
    }
}

async fn connect(fixture: &A2aFixture) -> A2aClient {
    let config = A2aAgentConfig::new("nous", fixture.base()).with_limits(limits());
    A2aClient::connect(&config).await.expect("connect")
}

fn configuration() -> SendMessageConfiguration {
    SendMessageConfiguration {
        accepted_output_modes: None,
        task_push_notification_config: None,
        history_length: None,
        return_immediately: None,
    }
}

fn hello() -> Message {
    Message::new(Role::User, vec![Part::text("hello")])
}

async fn collect(stream: A2aEventStream) -> Vec<Result<StreamResponse, A2aError>> {
    stream.collect().await
}

fn a_task_working_then_done() -> Vec<serde_json::Value> {
    vec![
        json!({"task": task_json("t1", "c1", "TASK_STATE_WORKING")}),
        status_event("TASK_STATE_WORKING", Some("reading the code")),
        json!({"artifactUpdate": {"taskId": "t1", "contextId": "c1", "artifact": {
            "artifactId": "a1", "parts": [{"text": "42"}],
        }}}),
        status_event("TASK_STATE_COMPLETED", None),
    ]
}

#[tokio::test]
async fn a_streamed_message_yields_every_event_in_order() {
    for binding in BINDINGS {
        let fixture = A2aFixture::start(Script {
            stream: RpcAnswer::Events(a_task_working_then_done(), StreamEnd::Close),
            ..Script::over(binding)
        });
        let client = connect(&fixture).await;

        let stream = client
            .send_streaming_message(hello(), configuration())
            .await
            .expect("open");
        let events = collect(stream).await;
        assert_eq!(events.len(), 4, "{binding}: {events:?}");
        assert!(matches!(&events[0], Ok(StreamResponse::Task(task)) if task.id == "t1"));
        assert!(matches!(&events[2], Ok(StreamResponse::ArtifactUpdate(_))));
        let Ok(StreamResponse::StatusUpdate(last)) = &events[3] else {
            panic!("{binding}: expected a status update, got {:?}", events[3]);
        };
        assert_eq!(last.status.state, TaskState::Completed);

        let request = &fixture.requests()[1];
        assert_eq!(request.header("accept"), Some("text/event-stream"));
        assert_eq!(fixture.operations(), ["SendStreamingMessage"]);
        assert_eq!(
            fixture.params_of("SendStreamingMessage")[0]["message"]["parts"][0]["text"],
            json!("hello")
        );
    }
}

#[tokio::test]
async fn a_subscription_streams_the_named_task() {
    for binding in BINDINGS {
        let fixture = A2aFixture::start(Script {
            subscribe: vec![RpcAnswer::Events(
                a_task_working_then_done(),
                StreamEnd::Close,
            )],
            ..Script::over(binding)
        });
        let client = connect(&fixture).await;

        let events = collect(client.subscribe_to_task("t1").await.expect("open")).await;
        assert!(events.iter().all(Result::is_ok), "{binding}: {events:?}");
        assert_eq!(events.len(), 4);
        assert_eq!(fixture.operations(), ["SubscribeToTask"]);
        if binding == A2aBinding::HttpJson {
            let request = &fixture.requests()[1];
            assert_eq!(
                (request.method.as_str(), request.target.as_str()),
                ("GET", "/rest/tasks/t1:subscribe")
            );
        }
    }
}

#[tokio::test]
async fn an_event_about_another_task_ends_the_stream() {
    for binding in BINDINGS {
        let fixture = A2aFixture::start(Script {
            subscribe: vec![RpcAnswer::Events(
                vec![
                    json!({"task": task_json("t1", "c1", "TASK_STATE_WORKING")}),
                    json!({"task": task_json("t2", "c1", "TASK_STATE_WORKING")}),
                    status_event("TASK_STATE_COMPLETED", None),
                ],
                StreamEnd::Close,
            )],
            ..Script::over(binding)
        });
        let client = connect(&fixture).await;

        let events = collect(client.subscribe_to_task("t1").await.expect("open")).await;
        assert_eq!(events.len(), 2, "{binding}: {events:?}");
        assert!(matches!(
            events[1],
            Err(A2aError::MismatchedTask {
                method: "SubscribeToTask"
            })
        ));
    }
}

#[tokio::test]
async fn a_stream_that_falls_silent_ends_at_the_idle_timeout() {
    for binding in BINDINGS {
        let fixture = A2aFixture::start(Script {
            subscribe: vec![RpcAnswer::Events(
                vec![json!({"task": task_json("t1", "c1", "TASK_STATE_WORKING")})],
                StreamEnd::Hang,
            )],
            ..Script::over(binding)
        });
        let client = connect(&fixture).await;

        let events = collect(client.subscribe_to_task("t1").await.expect("open")).await;
        assert_eq!(events.len(), 2, "{binding}: {events:?}");
        assert!(events[0].is_ok());
        assert!(
            matches!(events[1], Err(A2aError::StreamIdle(idle)) if idle == Duration::from_millis(200))
        );
    }
}

#[tokio::test]
async fn an_oversized_event_ends_the_stream() {
    let fixture = A2aFixture::start(Script {
        subscribe: vec![RpcAnswer::RawEvents(
            format!("data: {}\n\n", "x".repeat(4096)),
            StreamEnd::Close,
        )],
        ..Script::over(A2aBinding::HttpJson)
    });
    let config = A2aAgentConfig::new("nous", fixture.base()).with_limits(A2aLimits {
        max_event_bytes: 1024,
        ..limits()
    });
    let client = A2aClient::connect(&config).await.expect("connect");

    let events = collect(client.subscribe_to_task("t1").await.expect("open")).await;
    assert_eq!(events.len(), 1);
    assert!(matches!(
        events[0],
        Err(A2aError::EventTooLarge { limit: 1024 })
    ));
}

#[tokio::test]
async fn an_error_mid_stream_is_typed_without_server_text() {
    let jsonrpc_error = json!({"jsonrpc": "2.0", "id": 1, "error": {
        "code": -32004, "message": "\u{1b}[31mserver text that must not surface",
    }});
    for (binding, error) in [
        (A2aBinding::JsonRpc, jsonrpc_error),
        (A2aBinding::HttpJson, rest_error(-32004)),
        (
            A2aBinding::HttpJson,
            json!({"code": -32004, "message": "server text that must not surface"}),
        ),
    ] {
        let fixture = A2aFixture::start(Script {
            subscribe: vec![RpcAnswer::RawEvents(
                format!("data: {error}\n\n"),
                StreamEnd::Close,
            )],
            ..Script::over(binding)
        });
        let client = connect(&fixture).await;

        let events = collect(client.subscribe_to_task("t1").await.expect("open")).await;
        let [Err(error)] = events.as_slice() else {
            panic!("{binding}: expected one error, got {events:?}");
        };
        assert!(
            matches!(
                error,
                A2aError::Rpc {
                    method: "SubscribeToTask",
                    error: A2aRpcError::UnsupportedOperation
                }
            ),
            "{binding}: {error}"
        );
        assert!(!error.to_string().contains("server text"));
    }
}

#[tokio::test]
async fn a_refusal_instead_of_a_stream_is_reported_as_itself() {
    for binding in BINDINGS {
        let fixture = A2aFixture::start(Script {
            subscribe: vec![RpcAnswer::Error(-32004)],
            ..Script::over(binding)
        });
        let client = connect(&fixture).await;
        let error = client.subscribe_to_task("t1").await.expect_err("refused");
        assert!(
            matches!(
                error,
                A2aError::Rpc {
                    error: A2aRpcError::UnsupportedOperation,
                    ..
                }
            ),
            "{binding}: {error}"
        );

        let fixture = A2aFixture::start(Script {
            subscribe: vec![RpcAnswer::Result(task_json(
                "t1",
                "c1",
                "TASK_STATE_WORKING",
            ))],
            ..Script::over(binding)
        });
        let client = connect(&fixture).await;
        let error = client.subscribe_to_task("t1").await.expect_err("no stream");
        assert!(
            matches!(
                error,
                A2aError::NotAStream {
                    method: "SubscribeToTask"
                }
            ),
            "{binding}: {error}"
        );
    }
}

#[tokio::test]
async fn a_streamed_send_that_may_have_been_accepted_is_indeterminate() {
    for binding in BINDINGS {
        for (answer, indeterminate) in [
            (RpcAnswer::Http(502, "bad gateway".to_string()), true),
            (
                RpcAnswer::Result(json!({"task": task_json("t1", "c1", "TASK_STATE_WORKING")})),
                true,
            ),
            (RpcAnswer::Error(-32602), false),
        ] {
            let fixture = A2aFixture::start(Script {
                stream: answer.clone(),
                ..Script::over(binding)
            });
            let client = connect(&fixture).await;
            let error = client
                .send_streaming_message(hello(), configuration())
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
