use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;

use crate::{
    BuiltinProvider, ContentBlock, Message, ReasoningEffort, ReasoningOptions, Role, Runtime,
    agent::{
        ReasoningChange, RoundAdjustment, RoundBoundary, RoundContext, RoundDecision,
        RoundStrategy, RoundToolResult,
    },
    error::RuntimeError,
    runtime::RunOptions,
};

use super::support::{ScriptedProvider, StaticTool, model_info, text_stream, tool_use_stream};

/// One boundary observation recorded by [`ScriptedStrategy`].
#[derive(Clone)]
struct Observation {
    boundary: RoundBoundary,
    rounds_completed: usize,
    model_requests: usize,
    assistant_text: Option<String>,
    tool_results: Vec<RoundToolResult>,
}

/// A scripted decision returned at a boundary. An exhausted script yields
/// [`RoundDecision::proceed`].
enum DecisionScript {
    Inject(String),
    Stop,
    Switch(RoundAdjustment),
}

/// A [`RoundStrategy`] that records every boundary it observes and replays a
/// scripted sequence of decisions.
struct ScriptedStrategy {
    log: Mutex<Vec<Observation>>,
    decisions: Mutex<VecDeque<DecisionScript>>,
}

impl ScriptedStrategy {
    fn new(decisions: Vec<DecisionScript>) -> Arc<Self> {
        Arc::new(Self {
            log: Mutex::new(Vec::new()),
            decisions: Mutex::new(decisions.into()),
        })
    }

    fn observations(&self) -> Vec<Observation> {
        self.log.lock().expect("strategy log poisoned").clone()
    }

    fn invocation_count(&self) -> usize {
        self.log.lock().expect("strategy log poisoned").len()
    }
}

#[async_trait]
impl RoundStrategy for ScriptedStrategy {
    async fn on_round(&self, ctx: RoundContext<'_>) -> RoundDecision {
        self.log
            .lock()
            .expect("strategy log poisoned")
            .push(Observation {
                boundary: ctx.boundary(),
                rounds_completed: ctx.rounds_completed(),
                model_requests: ctx.model_requests(),
                assistant_text: ctx.assistant_message().map(Message::text),
                tool_results: ctx.tool_results().to_vec(),
            });
        match self
            .decisions
            .lock()
            .expect("strategy decisions poisoned")
            .pop_front()
        {
            None => RoundDecision::proceed(),
            Some(DecisionScript::Inject(text)) => {
                RoundDecision::inject(vec![ContentBlock::text(text)])
            }
            Some(DecisionScript::Stop) => RoundDecision::stop(),
            Some(DecisionScript::Switch(adjust)) => RoundDecision::Continue(adjust),
        }
    }
}

/// Captured outcome of a two-round probe session (tool round then text round).
struct SessionCapture {
    history: Vec<Message>,
    request_models: Vec<String>,
    request_messages: Vec<Vec<Message>>,
}

/// Runs the canonical two-round probe session (one tool round, one terminal text
/// round), optionally attaching a proceed-everywhere strategy built from
/// `decisions`, and captures the transcript and recorded requests.
async fn run_probe_session(decisions: Option<Vec<DecisionScript>>) -> SessionCapture {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "call-1", "probe_tool", r#"{"value":"hi"}"#),
            text_stream(&model.id, "done"),
        ],
    );
    let provider_handle = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("probe_tool", "ok"))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    let options = match decisions {
        Some(decisions) => {
            RunOptions::default().with_round_strategy(ScriptedStrategy::new(decisions))
        }
        None => RunOptions::default(),
    };
    agent
        .run(vec![ContentBlock::text("hi")], options)
        .await
        .expect("run succeeds");

    let requests = provider_handle.recorded_requests().await;
    SessionCapture {
        history: agent.history().to_vec(),
        request_models: requests.iter().map(|r| r.model.to_string()).collect(),
        request_messages: requests.iter().map(|r| r.messages.to_vec()).collect(),
    }
}

#[tokio::test]
async fn strategy_observes_both_round_boundaries_in_order() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "call-1", "probe_tool", r#"{"value":"hi"}"#),
            text_stream(&model.id, "done"),
        ],
    );
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("probe_tool", "ok"))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    let strategy = ScriptedStrategy::new(vec![]);
    agent
        .run(
            vec![ContentBlock::text("hi")],
            RunOptions::default().with_round_strategy(strategy.clone()),
        )
        .await
        .expect("run succeeds");

    let observations = strategy.observations();
    assert_eq!(observations.len(), 2);

    // Boundary (a): fired after the committed tool round, before the next round.
    assert_eq!(
        observations[0].boundary,
        RoundBoundary::ToolResultsCommitted
    );
    assert_eq!(observations[0].rounds_completed, 1);
    assert_eq!(observations[0].model_requests, 1);
    assert!(observations[0].assistant_text.is_none());
    assert_eq!(observations[0].tool_results.len(), 1);
    assert_eq!(observations[0].tool_results[0].tool_use_id, "call-1");
    assert_eq!(observations[0].tool_results[0].tool_name, "probe_tool");
    assert!(!observations[0].tool_results[0].is_error);

    // Boundary (b): fired after the committed tool-free assistant message.
    assert_eq!(
        observations[1].boundary,
        RoundBoundary::AssistantMessageCommitted
    );
    assert_eq!(observations[1].rounds_completed, 2);
    assert_eq!(observations[1].model_requests, 2);
    assert_eq!(observations[1].assistant_text.as_deref(), Some("done"));
    assert!(observations[1].tool_results.is_empty());
}

#[tokio::test]
async fn none_strategy_matches_continue_strategy_byte_identical() {
    // The `None` default and a proceed-everywhere strategy must produce an
    // identical transcript and identical recorded requests: the seam is inert.
    let baseline = run_probe_session(None).await;
    let with_strategy = run_probe_session(Some(vec![])).await;

    assert_eq!(baseline.request_models.len(), 2, "two rounds ran");
    assert_eq!(baseline.history, with_strategy.history);
    assert_eq!(baseline.request_models, with_strategy.request_models);
    assert_eq!(baseline.request_messages, with_strategy.request_messages);
}

#[tokio::test]
async fn injected_context_reaches_next_provider_request() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            text_stream(&model.id, "first"),
            text_stream(&model.id, "second"),
        ],
    );
    let provider_handle = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    let strategy = ScriptedStrategy::new(vec![DecisionScript::Inject(
        "please call finish_investigation".to_string(),
    )]);
    let message = agent
        .run(
            vec![ContentBlock::text("hi")],
            RunOptions::default().with_round_strategy(strategy),
        )
        .await
        .expect("run succeeds");

    assert_eq!(message.text(), "second");

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 2, "the injection forced a second round");
    let second_round_has_injection = requests[1]
        .messages
        .iter()
        .any(|m| m.role == Role::User && m.text().contains("please call finish_investigation"));
    assert!(
        second_round_has_injection,
        "injected corrective context must reach the next provider request"
    );
}

#[tokio::test]
async fn model_and_reasoning_switch_applies_to_next_round() {
    let model_a = model_info("model-a", BuiltinProvider::Anthropic);
    let model_b = model_info("model-b", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model_a.clone(), model_b.clone()],
        vec![
            tool_use_stream(&model_a.id, "call-1", "probe_tool", r#"{"value":"hi"}"#),
            text_stream(&model_b.id, "done"),
        ],
    );
    let provider_handle = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("probe_tool", "ok"))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model_a).expect("spawn agent");

    let adjust = RoundAdjustment::new()
        .with_model(model_b)
        .with_reasoning(ReasoningChange::Set(ReasoningOptions {
            effort: Some(ReasoningEffort::High),
            summary: None,
        }));
    let strategy = ScriptedStrategy::new(vec![DecisionScript::Switch(adjust)]);
    agent
        .run(
            vec![ContentBlock::text("hi")],
            RunOptions::default().with_round_strategy(strategy),
        )
        .await
        .expect("run succeeds");

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 2);
    // Round 1 (before the switch) uses the original model and no reasoning.
    assert_eq!(requests[0].model.as_ref(), "model-a");
    assert_eq!(requests[0].provider_request_options.reasoning, None);
    // Round 2 (after the switch) uses the switched model and reasoning.
    assert_eq!(requests[1].model.as_ref(), "model-b");
    assert_eq!(
        requests[1].provider_request_options.reasoning,
        Some(ReasoningOptions {
            effort: Some(ReasoningEffort::High),
            summary: None,
        })
    );
}

#[tokio::test]
async fn stop_after_tool_round_commits_transcript_and_halts() {
    // A Stop returned at the tool-round boundary matches `RunOptions::stop`: the
    // gathered transcript is committed (not rolled back) and no further model
    // request is made. Because the last committed message is a tool result rather
    // than an assistant message, `Agent::run` surfaces `EmptyAssistantResponse` —
    // the honest "stopped before a final answer" outcome, identical to
    // `RunOptions::stop` firing at the same boundary.
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "call-1", "probe_tool", r#"{"value":"hi"}"#),
            text_stream(&model.id, "must not run"),
        ],
    );
    let provider_handle = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("probe_tool", "ok"))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    let strategy = ScriptedStrategy::new(vec![DecisionScript::Stop]);
    let result = agent
        .run(
            vec![ContentBlock::text("go")],
            RunOptions::default().with_round_strategy(strategy),
        )
        .await;

    assert!(matches!(result, Err(RuntimeError::EmptyAssistantResponse)));
    assert_eq!(
        agent.history().len(),
        3,
        "the gathered tool round is committed, not rolled back"
    );
    assert_eq!(
        provider_handle.recorded_requests().await.len(),
        1,
        "the stop halted the run before a second model request"
    );
}

#[tokio::test]
async fn stop_at_assistant_boundary_returns_committed_message() {
    // At the assistant boundary a Stop commits the transcript and returns Ok with
    // the terminal message (the finish_run path, not rollback).
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![text_stream(&model.id, "final answer")],
    );
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    let strategy = ScriptedStrategy::new(vec![DecisionScript::Stop]);
    let message = agent
        .run(
            vec![ContentBlock::text("hi")],
            RunOptions::default().with_round_strategy(strategy),
        )
        .await
        .expect("a stop at the assistant boundary returns Ok");

    assert_eq!(message.text(), "final answer");
    assert_eq!(
        agent.history().len(),
        2,
        "user + committed assistant message"
    );
}

#[tokio::test]
async fn strategy_state_does_not_outlive_run() {
    // Two sequential runs on one runtime/agent, each with its own strategy
    // instance, share nothing: each strategy observes only its own run's boundary.
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            text_stream(&model.id, "first"),
            text_stream(&model.id, "second"),
        ],
    );
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    let strategy_one = ScriptedStrategy::new(vec![]);
    agent
        .run(
            vec![ContentBlock::text("run one")],
            RunOptions::default().with_round_strategy(strategy_one.clone()),
        )
        .await
        .expect("first run succeeds");

    let strategy_two = ScriptedStrategy::new(vec![]);
    agent
        .run(
            vec![ContentBlock::text("run two")],
            RunOptions::default().with_round_strategy(strategy_two.clone()),
        )
        .await
        .expect("second run succeeds");

    assert_eq!(
        strategy_one.invocation_count(),
        1,
        "the first strategy saw only its own run"
    );
    assert_eq!(
        strategy_two.invocation_count(),
        1,
        "the second strategy saw only its own run"
    );
}

#[tokio::test]
async fn assistant_boundary_continue_returns_inject_runs_another_round() {
    // Continue at the assistant boundary accepts the terminal message and returns.
    {
        let model = model_info("model", BuiltinProvider::Anthropic);
        let provider = ScriptedProvider::new(
            BuiltinProvider::Anthropic,
            vec![model.clone()],
            vec![text_stream(&model.id, "solo")],
        );
        let provider_handle = provider.clone();
        let runtime = Runtime::empty_builder()
            .with_provider_instance(provider)
            .build()
            .expect("build runtime");
        let mut agent = runtime.spawn("agent", model).expect("spawn agent");

        let strategy = ScriptedStrategy::new(vec![]);
        let message = agent
            .run(
                vec![ContentBlock::text("hi")],
                RunOptions::default().with_round_strategy(strategy),
            )
            .await
            .expect("run succeeds");

        assert_eq!(message.text(), "solo");
        assert_eq!(
            provider_handle.recorded_requests().await.len(),
            1,
            "continue returns without another round"
        );
    }

    // Inject at the assistant boundary prevents returning and runs another round.
    {
        let model = model_info("model", BuiltinProvider::Anthropic);
        let provider = ScriptedProvider::new(
            BuiltinProvider::Anthropic,
            vec![model.clone()],
            vec![
                text_stream(&model.id, "first"),
                text_stream(&model.id, "second"),
            ],
        );
        let provider_handle = provider.clone();
        let runtime = Runtime::empty_builder()
            .with_provider_instance(provider)
            .build()
            .expect("build runtime");
        let mut agent = runtime.spawn("agent", model).expect("spawn agent");

        let strategy =
            ScriptedStrategy::new(vec![DecisionScript::Inject("keep going".to_string())]);
        let message = agent
            .run(
                vec![ContentBlock::text("hi")],
                RunOptions::default().with_round_strategy(strategy),
            )
            .await
            .expect("run succeeds");

        assert_eq!(
            message.text(),
            "second",
            "the injected round produced the terminal message"
        );
        assert_eq!(
            provider_handle.recorded_requests().await.len(),
            2,
            "inject forced another model round"
        );
    }
}
