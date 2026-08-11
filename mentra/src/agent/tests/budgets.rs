use crate::{
    BuiltinProvider, ContentBlock, Role, Runtime, TokenUsage,
    agent::AgentEvent,
    error::RuntimeError,
    provider::{ContentBlockDelta, ContentBlockStart, ProviderEvent},
    runtime::{CancellationToken, RunOptions},
};

use super::support::{
    ScriptedProvider, StaticTool, StopTrippingTool, StreamScript, model_info, ok_stream,
};

/// Builds a [`TokenUsage`] reporting only `input_tokens`/`output_tokens`, the two
/// fields [`RunOptions::token_budget`] is evaluated against.
fn usage(input_tokens: u64, output_tokens: u64) -> TokenUsage {
    TokenUsage {
        input_tokens: Some(input_tokens),
        output_tokens: Some(output_tokens),
        ..Default::default()
    }
}

/// Like `support::tool_use_stream`, but also reports `usage` via `MessageDelta`
/// so a round-boundary [`RunOptions::token_budget`] check has something to
/// evaluate.
fn tool_use_stream_with_usage(
    model: &str,
    id: &str,
    name: &str,
    input_json: &str,
    usage: TokenUsage,
) -> StreamScript {
    ok_stream(vec![
        ProviderEvent::MessageStarted {
            id: format!("msg-{id}"),
            model: model.to_string(),
            role: Role::Assistant,
        },
        ProviderEvent::ContentBlockStarted {
            index: 0,
            kind: ContentBlockStart::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
            },
        },
        ProviderEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::ToolUseInputJson(input_json.to_string()),
        },
        ProviderEvent::ContentBlockStopped { index: 0 },
        ProviderEvent::MessageDelta {
            stop_reason: None,
            usage: Some(usage),
        },
        ProviderEvent::MessageStopped,
    ])
}

/// Like `support::text_stream`, but also reports `usage` via `MessageDelta`.
fn text_stream_with_usage(model: &str, text: &str, usage: TokenUsage) -> StreamScript {
    ok_stream(vec![
        ProviderEvent::MessageStarted {
            id: format!("msg-{text}"),
            model: model.to_string(),
            role: Role::Assistant,
        },
        ProviderEvent::ContentBlockStarted {
            index: 0,
            kind: ContentBlockStart::Text,
        },
        ProviderEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::Text(text.to_string()),
        },
        ProviderEvent::ContentBlockStopped { index: 0 },
        ProviderEvent::MessageDelta {
            stop_reason: None,
            usage: Some(usage),
        },
        ProviderEvent::MessageStopped,
    ])
}

#[tokio::test]
async fn token_budget_stops_gracefully_after_the_round_that_crosses_it() {
    // Round 1 reports usage that reaches the budget exactly; round 2 (a text
    // response) must never be requested. Because the run stops before a final
    // assistant message, `Agent::run` reports `EmptyAssistantResponse` — the same
    // honest "stopped before a final answer" outcome `RunOptions::stop` produces
    // at the identical boundary — while the gathered tool round stays committed
    // rather than rolled back.
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream_with_usage(
                &model.id,
                "call-1",
                "probe_tool",
                r#"{"value":"hi"}"#,
                usage(60, 40),
            ),
            // Must never be requested: the budget trips before round 2 starts.
            text_stream_with_usage(&model.id, "must not run", usage(1, 1)),
        ],
    );
    let provider_handle = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("probe_tool", "ok"))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    let result = agent
        .run(
            vec![ContentBlock::text("go")],
            RunOptions {
                token_budget: Some(100),
                ..Default::default()
            },
        )
        .await;

    assert!(matches!(result, Err(RuntimeError::EmptyAssistantResponse)));
    assert_eq!(
        agent.history().len(),
        3,
        "the round that crossed the budget stays committed, not rolled back"
    );
    assert_eq!(
        provider_handle.recorded_requests().await.len(),
        1,
        "the budget halted the run before a second model request"
    );
}

#[tokio::test]
async fn absent_token_budget_ignores_reported_usage() {
    // With `token_budget: None` (the default), no amount of reported usage stops
    // the run early — the seam is inert, reproducing today's behavior exactly.
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream_with_usage(
                &model.id,
                "call-1",
                "probe_tool",
                r#"{"value":"hi"}"#,
                usage(10_000, 10_000),
            ),
            text_stream_with_usage(&model.id, "done", usage(10_000, 10_000)),
        ],
    );
    let provider_handle = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("probe_tool", "ok"))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    let message = agent
        .run(vec![ContentBlock::text("go")], RunOptions::default())
        .await
        .expect("run completes normally despite large reported usage");

    assert_eq!(message.text(), "done");
    assert_eq!(provider_handle.recorded_requests().await.len(), 2);
    assert_eq!(agent.history().len(), 4);
}

#[tokio::test]
async fn child_run_shares_cancellation_with_parent() {
    // `RunOptions::child` carries the parent's `cancellation` token forward, so a
    // parent cancel stops a child run threaded with the derived options — even
    // though the two runs are on different agents and never call into each other.
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![text_stream_with_usage(
            &model.id,
            "should not complete",
            usage(1, 1),
        )],
    );
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut child_agent = runtime.spawn("child", model).expect("spawn child agent");

    let cancellation = CancellationToken::default();
    let parent_options = RunOptions {
        cancellation: Some(cancellation.clone()),
        ..Default::default()
    };
    let child_options = parent_options.child();
    cancellation.cancel();

    let error = child_agent
        .run(vec![ContentBlock::text("go")], child_options)
        .await
        .expect_err("a cancelled parent token must stop the derived child run");

    assert!(matches!(error, RuntimeError::Cancelled));
}

#[tokio::test]
async fn child_usage_counts_toward_shared_token_budget() {
    // Parent and child share one token-accounting handle via `RunOptions::child`:
    // neither run's own usage alone crosses the budget, but their combined total
    // does, so the child's run stops gracefully at the shared bound.
    let model = model_info("model", BuiltinProvider::Anthropic);
    let parent_provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![text_stream_with_usage(
            &model.id,
            "parent done",
            usage(40, 20),
        )],
    );
    let parent_runtime = Runtime::empty_builder()
        .with_provider_instance(parent_provider)
        .build()
        .expect("build runtime");
    let mut parent_agent = parent_runtime
        .spawn("parent", model.clone())
        .expect("spawn parent");

    let parent_options = RunOptions {
        token_budget: Some(100),
        ..Default::default()
    };
    parent_agent
        .run(vec![ContentBlock::text("go")], parent_options.clone())
        .await
        .expect("parent run completes under budget");
    assert_eq!(
        parent_options.reported_tokens(),
        60,
        "parent alone stays under the shared bound"
    );

    let child_options = parent_options.child();
    assert_eq!(
        child_options.reported_tokens(),
        60,
        "the derived child starts from the parent's already-reported usage"
    );

    let child_provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            // parent(60) + child(50) = 110, crossing the shared bound of 100.
            tool_use_stream_with_usage(
                &model.id,
                "call-1",
                "probe_tool",
                r#"{"value":"hi"}"#,
                usage(30, 20),
            ),
            text_stream_with_usage(&model.id, "must not run", usage(1, 1)),
        ],
    );
    let child_provider_handle = child_provider.clone();
    let child_runtime = Runtime::empty_builder()
        .with_provider_instance(child_provider)
        .with_tool(StaticTool::success("probe_tool", "ok"))
        .build()
        .expect("build runtime");
    let mut child_agent = child_runtime.spawn("child", model).expect("spawn child");

    let result = child_agent
        .run(vec![ContentBlock::text("go")], child_options)
        .await;

    assert!(
        matches!(result, Err(RuntimeError::EmptyAssistantResponse)),
        "the child stops gracefully once the combined parent+child usage crosses the bound"
    );
    assert_eq!(
        child_provider_handle.recorded_requests().await.len(),
        1,
        "the shared bound halted the child before its second round"
    );
}

/// Drains the events an agent emitted during a finished run.
fn collect_events(receiver: &mut tokio::sync::broadcast::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        events.push(event);
    }
    events
}

/// The `input + output` totals of every [`AgentEvent::UsageReport`] in `events`,
/// in the order they were emitted.
fn reported_usage_totals(events: &[AgentEvent]) -> Vec<u64> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::UsageReport {
                input_tokens,
                output_tokens,
                ..
            } => Some(input_tokens + output_tokens),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn delegated_subagent_usage_counts_against_the_parent_token_budget() {
    // The `task` intrinsic is the one child run mentra drives itself: the model
    // asks for it from inside the parent's run. Running it on the parent's
    // `RunOptions::child` is what stops a model from delegating its way past the
    // budget its own run was given — the child reports into the parent's
    // accounting handle, and the parent ends at the next round boundary once the
    // combined total crosses the bound.
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            // Parent round 1 delegates, reporting 60 of the 100-token bound.
            tool_use_stream_with_usage(
                &model.id,
                "parent-task",
                "task",
                r#"{"prompt":"delegate"}"#,
                usage(40, 20),
            ),
            // The delegated run spends 50 more, taking the shared total to 110.
            text_stream_with_usage(&model.id, "child summary", usage(30, 20)),
            // Parent round 2 must never be requested.
            text_stream_with_usage(&model.id, "parent done", usage(1, 1)),
        ],
    );
    let provider_handle = provider.clone();
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    let options = RunOptions {
        token_budget: Some(100),
        ..Default::default()
    };
    let result = agent
        .run(vec![ContentBlock::text("delegate that")], options.clone())
        .await;

    assert_eq!(
        options.reported_tokens(),
        110,
        "the delegated run must report into the parent's accounting handle, not a fresh one"
    );
    assert!(
        matches!(result, Err(RuntimeError::EmptyAssistantResponse)),
        "the parent stops gracefully at the boundary after delegated spend crossed the bound, \
         reporting the same 'stopped before a final answer' outcome any tripped bound does"
    );
    assert_eq!(
        provider_handle.recorded_requests().await.len(),
        2,
        "one parent round and one delegated round: the parent never got a second round"
    );
}

#[tokio::test]
async fn parent_cancellation_reaches_the_delegated_subagent() {
    // The delegated run shares the parent's cancellation token, so it ends at
    // its own next round boundary rather than running on unreachable while the
    // parent is torn down. `cancel_probe` trips the very token the parent's
    // options hold, so the child honoring it can only mean it inherited that
    // token rather than a default `None`.
    let model = model_info("model", BuiltinProvider::Anthropic);
    let cancellation = CancellationToken::default();
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream_with_usage(
                &model.id,
                "parent-task",
                "task",
                r#"{"prompt":"delegate"}"#,
                usage(1, 1),
            ),
            tool_use_stream_with_usage(
                &model.id,
                "child-tool",
                "cancel_probe",
                r#"{"value":"trip it"}"#,
                usage(1, 1),
            ),
            // Never requested: the child checks the shared token before its
            // second round.
            text_stream_with_usage(&model.id, "child must not continue", usage(1, 1)),
        ],
    );
    let provider_handle = provider.clone();
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .with_tool(StopTrippingTool::new("cancel_probe", cancellation.clone()))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    let error = agent
        .run(
            vec![ContentBlock::text("delegate that")],
            RunOptions {
                cancellation: Some(cancellation),
                ..Default::default()
            },
        )
        .await
        .expect_err("a cancelled run must fail rather than finish");

    assert!(matches!(error, RuntimeError::Cancelled));
    assert_eq!(
        provider_handle.recorded_requests().await.len(),
        2,
        "the child stopped at its own round boundary; without the shared token it would \
         have run a second round before the parent ever saw the cancellation"
    );
}

#[tokio::test]
async fn delegated_usage_reports_reach_the_parent_event_stream() {
    // The accounting fix alone would leave a parent's observer blind to
    // delegated spend, since a subagent has its own event bus. Relaying the
    // child's `UsageReport` keeps a stream that sums usage agreeing with the
    // shared handle the budget is checked against.
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream_with_usage(
                &model.id,
                "parent-task",
                "task",
                r#"{"prompt":"delegate"}"#,
                usage(40, 20),
            ),
            text_stream_with_usage(&model.id, "child summary", usage(30, 20)),
            text_stream_with_usage(&model.id, "parent done", usage(5, 5)),
        ],
    );
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");
    let mut events = agent.subscribe_events();

    let options = RunOptions::default();
    let message = agent
        .run(vec![ContentBlock::text("delegate that")], options.clone())
        .await
        .expect("the run completes with no bound set");

    assert_eq!(message.text(), "parent done");
    let totals = reported_usage_totals(&collect_events(&mut events));
    assert_eq!(
        totals,
        vec![60, 50, 10],
        "the parent's stream carries the delegated round's usage between its own two rounds"
    );
    assert_eq!(
        totals.iter().sum::<u64>(),
        options.reported_tokens(),
        "what an observer sums from the stream matches what the budget is checked against"
    );
}

#[tokio::test]
async fn delegating_with_the_budget_already_spent_fails_the_delegation() {
    // A round is always allowed to finish, so the round that crosses the bound
    // can still be the one asking to delegate. The child then inherits an
    // already-exceeded budget and does zero rounds. That surfaces as a failed
    // delegation the parent can see, not as a silent empty success — and the
    // provider is never called on the child's behalf.
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            // This single round spends the whole 100-token bound and delegates.
            tool_use_stream_with_usage(
                &model.id,
                "parent-task",
                "task",
                r#"{"prompt":"delegate"}"#,
                usage(60, 60),
            ),
            // Neither the child nor a second parent round may be requested.
            text_stream_with_usage(&model.id, "must not run", usage(1, 1)),
        ],
    );
    let provider_handle = provider.clone();
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    let result = agent
        .run(
            vec![ContentBlock::text("delegate that")],
            RunOptions {
                token_budget: Some(100),
                ..Default::default()
            },
        )
        .await;

    assert!(matches!(result, Err(RuntimeError::EmptyAssistantResponse)));
    assert_eq!(
        provider_handle.recorded_requests().await.len(),
        1,
        "the delegated run stopped at its first boundary without a model request"
    );
    let subagents = agent.watch_snapshot().borrow().subagents.clone();
    assert_eq!(subagents.len(), 1);
    assert!(
        matches!(
            &subagents[0].status,
            crate::agent::SpawnedAgentStatus::Failed(message)
                if message == "run completed without a final assistant message"
        ),
        "the exhausted delegation is recorded as failed, not finished: {:?}",
        subagents[0].status
    );
}
