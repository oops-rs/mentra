use crate::{
    BuiltinProvider, ContentBlock, Role, Runtime, TokenUsage,
    error::RuntimeError,
    provider::{ContentBlockDelta, ContentBlockStart, ProviderEvent},
    runtime::{CancellationToken, RunOptions},
};

use super::support::{ScriptedProvider, StaticTool, StreamScript, model_info, ok_stream};

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
