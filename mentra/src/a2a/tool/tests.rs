use std::sync::Mutex;

use serde_json::json;

use super::*;
use crate::a2a::testing::{A2aFixture, RpcAnswer, Script, standard_card, task_json};
use crate::a2a::{A2aAgentConfig, A2aBinding, A2aLimits};
use crate::runtime::CancellationToken;
use crate::test::{MockRuntime, MockToolCall};
use crate::{ContentBlock, tool::ToolResultContent};

fn fast_limits() -> A2aLimits {
    A2aLimits {
        poll_interval: Duration::from_millis(10),
        max_wait: Duration::from_secs(5),
        ..A2aLimits::default()
    }
}

async fn tool_with(fixture: &A2aFixture, limits: A2aLimits) -> A2aAgentTool {
    let config = A2aAgentConfig::new("nous", fixture.base()).with_limits(limits);
    let client = A2aClient::connect(&config).await.expect("connect");
    A2aAgentTool::new(Arc::new(client))
}

async fn tool(fixture: &A2aFixture) -> A2aAgentTool {
    tool_with(fixture, fast_limits()).await
}

fn send(text: &str) -> Call {
    Call::Send {
        text: text.to_string(),
        task_id: None,
        context_id: None,
    }
}

async fn run(tool: &A2aAgentTool, call: Call) -> Result<ToolOutput, String> {
    tool.run(call, &RunOptions::default(), &|_| {}).await
}

fn text(output: &ToolOutput) -> String {
    output.content.to_string()
}

fn with_artifact(mut task: Value, parts: Value) -> Value {
    task["artifacts"] = json!([{"artifactId": "a1", "parts": parts}]);
    task
}

fn with_status_message(mut task: Value, text: &str) -> Value {
    task["status"]["message"] = json!({
        "messageId": "m1",
        "role": "ROLE_AGENT",
        "parts": [{"text": text}],
    });
    task
}

fn working() -> RpcAnswer {
    RpcAnswer::Result(task_json("t1", "c1", "TASK_STATE_WORKING"))
}

#[tokio::test]
async fn a_message_is_submitted_then_polled_to_completion() {
    let completed = with_artifact(
        task_json("t1", "c1", "TASK_STATE_COMPLETED"),
        json!([{"text": "the answer is 42"}]),
    );
    let fixture = A2aFixture::start(Script {
        send: RpcAnswer::Result(json!({"task": task_json("t1", "c1", "TASK_STATE_SUBMITTED")})),
        get: vec![working(), RpcAnswer::Result(completed)],
        ..Script::default()
    });
    let tool = tool(&fixture).await;

    let output = run(&tool, send("what is the answer?"))
        .await
        .expect("completed");
    let text = text(&output);
    assert!(
        text.starts_with(
            "Remote agent `nous` (A2A) replied:\nstate: completed\ntask_id: t1\ncontext_id: c1\n"
        ),
        "{text}"
    );
    assert!(
        text.contains(
            "--- begin output from remote agent `nous` ---\nthe answer is 42\n\
             --- end output from remote agent `nous` ---"
        ),
        "{text}"
    );
    assert_eq!(
        output.details,
        Some(json!({"task_id": "t1", "context_id": "c1", "state": "completed"}))
    );

    assert_eq!(fixture.operations(), ["SendMessage", "GetTask", "GetTask"]);
    let sent = &fixture.params_of("SendMessage")[0];
    assert_eq!(sent["configuration"]["returnImmediately"], json!(true));
    assert_eq!(
        sent["message"]["parts"][0]["text"],
        json!("what is the answer?")
    );
    assert_eq!(fixture.params_of("GetTask")[0]["id"], json!("t1"));
}

#[tokio::test]
async fn a_direct_message_reply_is_returned_without_polling() {
    let fixture = A2aFixture::start(Script {
        send: RpcAnswer::Result(json!({"message": {
            "messageId": "m1",
            "contextId": "c9",
            "role": "ROLE_AGENT",
            "parts": [{"text": "hello back"}],
        }})),
        ..Script::default()
    });
    let tool = tool(&fixture).await;

    let output = run(&tool, send("hello")).await.expect("reply");
    let text = text(&output);
    assert!(
        text.starts_with("Remote agent `nous` (A2A) replied:\ncontext_id: c9\n"),
        "{text}"
    );
    assert!(text.contains("hello back"), "{text}");
    assert_eq!(fixture.operations(), ["SendMessage"]);
}

#[tokio::test]
async fn an_input_required_task_surfaces_its_question_and_ids() {
    let asking = with_status_message(
        task_json("t1", "c1", "TASK_STATE_INPUT_REQUIRED"),
        "Which branch should I look at?",
    );
    let fixture = A2aFixture::start(Script {
        send: RpcAnswer::Result(json!({"task": asking})),
        ..Script::default()
    });
    let tool = tool(&fixture).await;

    let output = run(&tool, send("investigate the bug"))
        .await
        .expect("interrupted is not an error");
    let text = text(&output);
    assert!(
        text.starts_with("Remote agent `nous` (A2A) needs input."),
        "{text}"
    );
    assert!(
        text.contains("state: input-required\ntask_id: t1\ncontext_id: c1"),
        "{text}"
    );
    assert!(
        text.contains("call `a2a__nous` again with this `task_id`"),
        "{text}"
    );
    assert!(text.contains("Which branch should I look at?"), "{text}");
    assert_eq!(fixture.operations(), ["SendMessage"]);
}

#[tokio::test]
async fn an_answer_to_a_waiting_task_names_the_task() {
    let fixture = A2aFixture::start(Script::default());
    let tool = tool(&fixture).await;

    run(
        &tool,
        Call::Send {
            text: "main".to_string(),
            task_id: Some("t1".to_string()),
            context_id: Some("c1".to_string()),
        },
    )
    .await
    .expect("completed");
    let message = &fixture.params_of("SendMessage")[0]["message"];
    assert_eq!(message["taskId"], json!("t1"));
    assert_eq!(message["contextId"], json!("c1"));
}

#[tokio::test]
async fn a_task_still_working_at_the_bound_returns_its_ids_and_resuming_does_not_resend() {
    let fixture = A2aFixture::start(Script {
        send: RpcAnswer::Result(json!({"task": task_json("t1", "c1", "TASK_STATE_WORKING")})),
        get: vec![RpcAnswer::Result(with_artifact(
            task_json("t1", "c1", "TASK_STATE_COMPLETED"),
            json!([{"text": "done at last"}]),
        ))],
        ..Script::default()
    });

    // A zero wait hands the task back as soon as it exists.
    let impatient = tool_with(
        &fixture,
        A2aLimits {
            max_wait: Duration::ZERO,
            ..fast_limits()
        },
    )
    .await;
    let output = run(&impatient, send("long job"))
        .await
        .expect("handed back");
    let first = text(&output);
    assert!(
        first.starts_with("Remote agent `nous` (A2A) is still working."),
        "{first}"
    );
    assert!(
        first.contains("state: working\ntask_id: t1\ncontext_id: c1"),
        "{first}"
    );
    assert!(
        first.contains("call `a2a__nous` again with only this `task_id`"),
        "{first}"
    );
    assert!(first.contains("do not resend the message"), "{first}");

    let resumed = run(
        &tool(&fixture).await,
        Call::Resume {
            task_id: "t1".to_string(),
        },
    )
    .await
    .expect("completed");
    assert!(text(&resumed).contains("done at last"));
    assert_eq!(fixture.operations(), ["SendMessage", "GetTask"]);
}

#[tokio::test]
async fn reaching_the_bound_while_polling_leaves_the_task_running() {
    let fixture = A2aFixture::start(Script {
        send: RpcAnswer::Result(json!({"task": task_json("t1", "c1", "TASK_STATE_WORKING")})),
        get: vec![working()],
        ..Script::default()
    });
    let tool = tool_with(
        &fixture,
        A2aLimits {
            max_wait: Duration::from_millis(60),
            ..fast_limits()
        },
    )
    .await;

    let output = run(&tool, send("long job")).await.expect("handed back");
    assert!(text(&output).contains("did not finish within 1s"));

    // Returning disarms the drop guard: no cancel follows the call.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let methods = fixture.operations();
    assert!(methods.iter().filter(|m| *m == "GetTask").count() >= 1);
    assert!(!methods.contains(&"CancelTask".to_string()), "{methods:?}");
}

#[tokio::test]
async fn a_failed_task_is_an_error_carrying_the_reason() {
    let failed = with_status_message(task_json("t1", "c1", "TASK_STATE_FAILED"), "disk full");
    let fixture = A2aFixture::start(Script {
        send: RpcAnswer::Result(json!({"task": failed})),
        ..Script::default()
    });
    let tool = tool(&fixture).await;

    let error = run(&tool, send("go")).await.expect_err("failed");
    assert!(error.contains("reports the task failed"), "{error}");
    assert!(error.contains("task_id: t1"), "{error}");
    assert!(error.contains("disk full"), "{error}");
}

#[tokio::test]
async fn resuming_an_unknown_task_reports_the_typed_error() {
    let fixture = A2aFixture::start(Script {
        get: vec![RpcAnswer::Error(-32001)],
        ..Script::default()
    });
    let tool = tool(&fixture).await;

    let error = run(
        &tool,
        Call::Resume {
            task_id: "gone".to_string(),
        },
    )
    .await
    .expect_err("not found");
    assert!(error.contains("remote agent `nous`"), "{error}");
    assert!(error.contains("TaskNotFoundError (-32001)"), "{error}");
    assert!(!error.contains("server text"), "{error}");
}

#[tokio::test]
async fn a_cancelled_run_cancels_the_remote_task() {
    let fixture = A2aFixture::start(Script {
        send: RpcAnswer::Result(json!({"task": task_json("t1", "c1", "TASK_STATE_WORKING")})),
        get: vec![working()],
        ..Script::default()
    });
    let tool = tool(&fixture).await;
    let token = CancellationToken::default();
    let options = RunOptions {
        cancellation: Some(token.clone()),
        ..RunOptions::default()
    };

    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();
    });
    let error = tool
        .run(send("long job"), &options, &|_| {})
        .await
        .expect_err("cancelled");
    canceller.await.expect("canceller");

    assert!(error.contains("run was cancelled"), "{error}");
    assert!(error.contains("asked to cancel"), "{error}");
    assert_eq!(fixture.params_of("CancelTask")[0]["id"], json!("t1"));
}

#[tokio::test]
async fn a_stopping_run_hands_the_task_back_without_cancelling_it() {
    let fixture = A2aFixture::start(Script {
        send: RpcAnswer::Result(json!({"task": task_json("t1", "c1", "TASK_STATE_WORKING")})),
        ..Script::default()
    });
    let tool = tool(&fixture).await;
    let stop = CancellationToken::default();
    stop.cancel();
    let options = RunOptions {
        stop: Some(stop),
        ..RunOptions::default()
    };

    let output = tool
        .run(send("long job"), &options, &|_| {})
        .await
        .expect("handed back");
    assert!(text(&output).contains("the run is ending"));
    assert_eq!(fixture.operations(), ["SendMessage"]);
}

#[tokio::test]
async fn a_dropped_call_cancels_the_remote_task() {
    let fixture = A2aFixture::start(Script {
        send: RpcAnswer::Result(json!({"task": task_json("t1", "c1", "TASK_STATE_WORKING")})),
        get: vec![working()],
        ..Script::default()
    });
    let tool = tool(&fixture).await;

    let dropped =
        tokio::time::timeout(Duration::from_millis(80), run(&tool, send("long job"))).await;
    assert!(dropped.is_err(), "the call should still be waiting");

    for _ in 0..50 {
        if fixture.operations().contains(&"CancelTask".to_string()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "no CancelTask after the call was dropped: {:?}",
        fixture.operations()
    );
}

#[tokio::test]
async fn progress_is_reported_when_the_state_changes() {
    let fixture = A2aFixture::start(Script {
        send: RpcAnswer::Result(json!({"task": task_json("t1", "c1", "TASK_STATE_SUBMITTED")})),
        get: vec![working(), working()],
        ..Script::default()
    });
    let tool = tool_with(
        &fixture,
        A2aLimits {
            max_wait: Duration::from_millis(50),
            ..fast_limits()
        },
    )
    .await;
    let seen = Mutex::new(Vec::new());

    tool.run(send("go"), &RunOptions::default(), &|text| {
        seen.lock().expect("progress log").push(text)
    })
    .await
    .expect("handed back");
    assert_eq!(
        *seen.lock().expect("progress log"),
        ["remote task t1 is working"]
    );
}

#[tokio::test]
async fn non_text_parts_are_summarized_not_dropped() {
    let completed = with_artifact(
        task_json("t1", "c1", "TASK_STATE_COMPLETED"),
        json!([
            {"text": "see attached"},
            {"raw": "YWJj", "filename": "report.pdf", "mediaType": "application/pdf"},
            {"url": "https://example.com/chart.png", "mediaType": "image/png"},
            {"data": {"rows": 3}, "mediaType": "application/json"},
        ]),
    );
    let fixture = A2aFixture::start(Script {
        send: RpcAnswer::Result(json!({"task": completed})),
        ..Script::default()
    });
    let tool = tool(&fixture).await;

    let text = text(&run(&tool, send("report")).await.expect("completed"));
    for expected in [
        "see attached",
        "[file report.pdf (application/pdf): 3 bytes inline, not shown]",
        "[file (image/png) at https://example.com/chart.png]",
        "[data (application/json)]\n{\n  \"rows\": 3\n}",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in {text}");
    }
}

#[tokio::test]
async fn the_remote_text_is_bounded() {
    let completed = with_artifact(
        task_json("t1", "c1", "TASK_STATE_COMPLETED"),
        json!([{"text": "é".repeat(10_000)}]),
    );
    let fixture = A2aFixture::start(Script {
        send: RpcAnswer::Result(json!({"task": completed})),
        ..Script::default()
    });
    let tool = tool_with(
        &fixture,
        A2aLimits {
            max_output_bytes: 101,
            ..fast_limits()
        },
    )
    .await;

    let text = text(&run(&tool, send("go")).await.expect("completed"));
    assert!(
        text.contains("[truncated: 19900 more bytes not shown]"),
        "{text}"
    );
    assert!(
        text.ends_with("--- end output from remote agent `nous` ---"),
        "{text}"
    );
    assert!(text.len() < 400, "{}", text.len());
}

#[test]
fn input_is_checked_before_anything_is_sent() {
    assert_eq!(Call::parse(json!({"message": "hi"})), Ok(send("hi")));
    assert_eq!(
        Call::parse(json!({"task_id": "t1"})),
        Ok(Call::Resume {
            task_id: "t1".to_string()
        })
    );
    for (input, complaint) in [
        (json!({}), "pass `message`"),
        (json!({"context_id": "c1"}), "pass `message`"),
        (json!({"task_id": "t1", "context_id": "c1"}), "only applies"),
        (json!({"message": "  "}), "must not be empty"),
        (json!({"message": "hi", "extra": 1}), "unknown field"),
    ] {
        let error = Call::parse(input.clone()).expect_err("invalid");
        assert!(error.contains(complaint), "{input} -> {error}");
    }
}

#[tokio::test]
async fn the_descriptor_declares_an_external_delegation() {
    let fixture = A2aFixture::start(Script::default());
    let descriptor = tool(&fixture).await.descriptor();

    assert_eq!(descriptor.provider.name, "a2a__nous");
    assert_eq!(descriptor.side_effect_level, ToolSideEffectLevel::External);
    assert_eq!(descriptor.durability, ToolDurability::Ephemeral);
    assert_eq!(
        descriptor.execution_category,
        ToolExecutionCategory::Delegation
    );
    assert_eq!(
        descriptor.approval_category,
        ToolApprovalCategory::Delegation
    );
    assert!(
        descriptor
            .capabilities
            .contains(&ToolCapability::Custom("a2a:nous".to_string()))
    );
    let description = descriptor.provider.description.clone().unwrap_or_default();
    assert!(description.contains("Investigates questions about the codebase."));
    assert!(description.contains("- Investigate: Answers a question by reading the code."));
    assert_eq!(
        descriptor.provider.input_schema["properties"]
            .as_object()
            .map(|properties| properties.keys().cloned().collect::<Vec<_>>()),
        Some(vec![
            "context_id".to_string(),
            "message".to_string(),
            "task_id".to_string()
        ])
    );
}

#[tokio::test]
async fn the_description_is_bounded_however_large_the_card() {
    let mut card = standard_card("http://127.0.0.1:1", A2aBinding::JsonRpc);
    card["description"] = json!("d".repeat(50_000));
    card["skills"] = json!(
        (0..100)
            .map(|index| json!({
                "id": format!("s{index}"),
                "name": format!("Skill {index}"),
                "description": "x".repeat(5_000),
                "tags": [],
            }))
            .collect::<Vec<_>>()
    );
    let fixture = A2aFixture::start(Script {
        card: Some(card),
        ..Script::default()
    });

    let description = tool(&fixture)
        .await
        .descriptor()
        .provider
        .description
        .unwrap_or_default();
    assert!(description.len() < 3_000, "{}", description.len());
    assert!(description.contains("never resend the message"));
}

#[tokio::test]
async fn the_tool_runs_through_the_runtime() {
    let completed = with_artifact(
        task_json("t1", "c1", "TASK_STATE_COMPLETED"),
        json!([{"text": "remote answer"}]),
    );
    let fixture = A2aFixture::start(Script {
        send: RpcAnswer::Result(json!({"task": completed})),
        ..Script::default()
    });
    let tool = tool(&fixture).await;

    let mock = MockRuntime::builder()
        .tool_calls([MockToolCall::new("a2a__nous", json!({"message": "ask"})).with_id("a2a-call")])
        .text("done")
        .build()
        .expect("build mock runtime");
    mock.runtime().register_tool(tool);
    let mut agent = mock
        .runtime()
        .spawn("a2a-runtime-test", mock.model())
        .expect("spawn agent");

    let response = agent
        .send(vec![ContentBlock::text("ask nous")])
        .await
        .expect("run agent");
    assert_eq!(response.text(), "done");

    let requests = mock.recorded_requests().await;
    let result = requests[1]
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } if tool_use_id == "a2a-call" => Some((content.clone(), *is_error)),
            _ => None,
        })
        .expect("tool result");
    assert!(!result.1);
    assert!(matches!(&result.0, ToolResultContent::Text(text) if text.contains("remote answer")));
}
