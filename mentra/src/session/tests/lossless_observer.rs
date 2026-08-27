//! Conformance tests for the lossless in-process agent-event observer exposed
//! by [`Session`](crate::Session).

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::{
    sync::{Notify, broadcast::error::TryRecvError},
    time::{Duration, timeout},
};

use crate::{
    ContentBlock, SessionEvent,
    agent::AgentEvent,
    error::RuntimeError,
    runtime::{
        CancellationToken, PostExecutionContext, PostExecutionHook, ResultDecision, RunOptions,
    },
    test::{MockRuntime, MockToolCall},
    tool::{
        ToolContext, ToolDefinition, ToolDurability, ToolExecutionCategory, ToolExecutor,
        ToolOutput, ToolResultContent, ToolSideEffectLevel, ToolSpec,
    },
};

const FAILED_RESULT: &str = "evidence tool failed with its complete diagnostic";
const OBSERVER_ONLY_TAIL: &str = "observer-only-structured-tail";
const CANCELLED_PARALLEL_TAIL: &str = "completed-before-sibling-cancellation";

struct StructuredEvidenceTool;

#[async_trait]
impl ToolDefinition for StructuredEvidenceTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("structured_evidence")
            .description("Return a structured evidence payload")
            .input_schema(json!({ "type": "object" }))
            .side_effect_level(ToolSideEffectLevel::LocalState)
            .durability(ToolDurability::Ephemeral)
            .execution_category(ToolExecutionCategory::ExclusiveLocalMutation)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for StructuredEvidenceTool {
    async fn execute_mut_output(
        &self,
        _ctx: ToolContext<'_>,
        _input: Value,
    ) -> Result<ToolOutput, String> {
        Ok(ToolOutput::structured(structured_result()))
    }
}

struct FailedEvidenceTool;

#[async_trait]
impl ToolDefinition for FailedEvidenceTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("failed_evidence")
            .description("Return a complete tool-level failure")
            .input_schema(json!({ "type": "object" }))
            .side_effect_level(ToolSideEffectLevel::LocalState)
            .durability(ToolDurability::Ephemeral)
            .execution_category(ToolExecutionCategory::ExclusiveLocalMutation)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for FailedEvidenceTool {
    async fn execute_mut_output(
        &self,
        _ctx: ToolContext<'_>,
        _input: Value,
    ) -> Result<ToolOutput, String> {
        Err(FAILED_RESULT.to_string())
    }
}

struct CompletedParallelEvidenceTool {
    blocking_started: Arc<Notify>,
}

#[async_trait]
impl ToolDefinition for CompletedParallelEvidenceTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("completed_parallel_evidence")
            .description("Complete while a sibling parallel tool remains pending")
            .input_schema(json!({ "type": "object" }))
            .side_effect_level(ToolSideEffectLevel::None)
            .durability(ToolDurability::ReplaySafe)
            .execution_category(ToolExecutionCategory::ReadOnlyParallel)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for CompletedParallelEvidenceTool {
    async fn execute_output(
        &self,
        _ctx: crate::tool::ParallelToolContext,
        _input: Value,
    ) -> Result<ToolOutput, String> {
        self.blocking_started.notified().await;
        Ok(ToolOutput::structured(cancelled_parallel_result()))
    }
}

struct BlockingParallelEvidenceTool {
    started: Arc<Notify>,
}

#[async_trait]
impl ToolDefinition for BlockingParallelEvidenceTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("blocking_parallel_evidence")
            .description("Remain pending until the parallel batch is cancelled")
            .input_schema(json!({ "type": "object" }))
            .side_effect_level(ToolSideEffectLevel::None)
            .durability(ToolDurability::ReplaySafe)
            .execution_category(ToolExecutionCategory::ReadOnlyParallel)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for BlockingParallelEvidenceTool {
    async fn execute_output(
        &self,
        _ctx: crate::tool::ParallelToolContext,
        _input: Value,
    ) -> Result<ToolOutput, String> {
        self.started.notify_one();
        std::future::pending().await
    }
}

struct CountsPostExecution(Arc<AtomicUsize>);

#[async_trait]
impl PostExecutionHook for CountsPostExecution {
    async fn post_tool_execution(
        &self,
        _context: &PostExecutionContext,
    ) -> Result<ResultDecision, RuntimeError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ResultDecision::Keep)
    }
}

fn structured_result() -> Value {
    // The tail is deliberately beyond the session stream's bounded summary.
    // The in-process observer must still receive it as structured JSON.
    json!({
        "payload": ["x".repeat(300), OBSERVER_ONLY_TAIL],
        "source_refs": ["doc:alpha#L1", "code:beta.rs#L2"]
    })
}

fn cancelled_parallel_result() -> Value {
    json!({
        "payload": ["y".repeat(300), CANCELLED_PARALLEL_TAIL],
        "source_refs": ["doc:parallel#L7"]
    })
}

#[tokio::test]
async fn observer_preserves_complete_tool_payloads_and_occurrence_order() {
    let structured_input = json!({
        "query": "why did the invariant fail?",
        "filters": { "scope": ["docs", "code"], "exact": true }
    });
    let failed_input = json!({ "evidence_id": "ev-2" });
    let mock = MockRuntime::builder()
        .tool_calls([
            MockToolCall::new("structured_evidence", structured_input.clone())
                .with_id("call-structured"),
            MockToolCall::new("failed_evidence", failed_input.clone()).with_id("call-failed"),
        ])
        .text("done")
        .build()
        .unwrap();
    mock.runtime().register_tool(StructuredEvidenceTool);
    mock.runtime().register_tool(FailedEvidenceTool);

    let mut session = mock
        .runtime()
        .create_session("lossless-observer", mock.model())
        .unwrap();
    let mut wire = session.subscribe();
    let observed = Arc::new(Mutex::new(Vec::<AgentEvent>::new()));
    let observed_for_tap = Arc::clone(&observed);
    let _tap = session.register_agent_event_tap(move |event| {
        observed_for_tap
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(event.clone());
    });

    let message = session
        .append_turn(vec![ContentBlock::text("collect both facts")])
        .await
        .unwrap();
    assert_eq!(message.text(), "done");

    let events = observed
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let lifecycle = events
        .iter()
        .filter_map(tool_lifecycle_label)
        .collect::<Vec<_>>();
    assert_eq!(
        lifecycle,
        [
            "ready:call-structured",
            "ready:call-failed",
            "started:call-structured",
            "finished:call-structured",
            "started:call-failed",
            "finished:call-failed",
        ],
        "the observer must preserve the event bus's occurrence order"
    );

    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolUseReady { call, .. }
            if call.id == "call-structured"
                && call.name == "structured_evidence"
                && call.input == structured_input
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolUseReady { call, .. }
            if call.id == "call-failed"
                && call.name == "failed_evidence"
                && call.input == failed_input
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolExecutionFinished {
            result: ContentBlock::ToolResult {
                tool_use_id,
                content: ToolResultContent::Structured(content),
                is_error: false,
            },
        } if tool_use_id == "call-structured" && content == &structured_result()
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolExecutionFinished {
            result: ContentBlock::ToolResult {
                tool_use_id,
                content: ToolResultContent::Text(content),
                is_error: true,
            },
        } if tool_use_id == "call-failed" && content == FAILED_RESULT
    )));

    let wire_events = std::iter::from_fn(|| wire.try_recv().ok()).collect::<Vec<_>>();
    let tool_completions = wire_events
        .iter()
        .filter(|event| matches!(event, SessionEvent::ToolCompleted { .. }))
        .collect::<Vec<_>>();
    assert_eq!(tool_completions.len(), 2);
    for event in tool_completions {
        let value = serde_json::to_value(event).unwrap();
        let object = value.as_object().unwrap();
        assert!(!object.contains_key("content"));
        assert!(!object.contains_key("result"));
        assert!(!object.contains_key("details"));
    }
    assert!(
        wire_events.iter().all(|event| !serde_json::to_string(event)
            .unwrap()
            .contains(OBSERVER_ONLY_TAIL)),
        "the public session stream may keep its bounded summary, but not the complete body"
    );
}

fn tool_lifecycle_label(event: &AgentEvent) -> Option<String> {
    match event {
        AgentEvent::ToolUseReady { call, .. } => Some(format!("ready:{}", call.id)),
        AgentEvent::ToolExecutionStarted { call } => Some(format!("started:{}", call.id)),
        AgentEvent::ToolExecutionFinished {
            result: ContentBlock::ToolResult { tool_use_id, .. },
        } => Some(format!("finished:{tool_use_id}")),
        _ => None,
    }
}

#[tokio::test]
async fn observer_does_not_lag_when_broadcast_receivers_overflow() {
    const DELTA_COUNT: usize = 700;
    let mock = MockRuntime::builder()
        .stream_text(std::iter::repeat_n("x", DELTA_COUNT))
        .build()
        .unwrap();
    let mut session = mock
        .runtime()
        .create_session("non-lagging-observer", mock.model())
        .unwrap();
    let mut wire = session.subscribe();
    let delta_count = Arc::new(AtomicUsize::new(0));
    let last_full_text = Arc::new(Mutex::new(String::new()));
    let delta_count_for_tap = Arc::clone(&delta_count);
    let last_full_text_for_tap = Arc::clone(&last_full_text);
    let _tap = session.register_agent_event_tap(move |event| {
        if let AgentEvent::TextDelta { delta, full_text } = event {
            assert_eq!(delta, "x");
            delta_count_for_tap.fetch_add(1, Ordering::SeqCst);
            *last_full_text_for_tap
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = full_text.clone();
        }
    });

    let message = session
        .append_turn(vec![ContentBlock::text("stream")])
        .await
        .unwrap();

    assert_eq!(message.text().len(), DELTA_COUNT);
    assert_eq!(delta_count.load(Ordering::SeqCst), DELTA_COUNT);
    assert_eq!(
        last_full_text
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len(),
        DELTA_COUNT
    );
    assert!(
        matches!(wire.try_recv(), Err(TryRecvError::Lagged(_))),
        "the bounded public broadcast should overflow in this fixture, proving the tap is independent"
    );
}

#[tokio::test]
async fn observer_keeps_a_parallel_result_when_cancellation_skips_post_hooks() {
    let post_execution_calls = Arc::new(AtomicUsize::new(0));
    let blocking_tool_started = Arc::new(Notify::new());
    let mock = MockRuntime::builder()
        .tool_calls([
            MockToolCall::new("completed_parallel_evidence", json!({ "id": 1 }))
                .with_id("call-completed"),
            MockToolCall::new("blocking_parallel_evidence", json!({ "id": 2 }))
                .with_id("call-blocking"),
        ])
        .with_post_hook(CountsPostExecution(Arc::clone(&post_execution_calls)))
        .build()
        .unwrap();
    mock.runtime().register_tool(CompletedParallelEvidenceTool {
        blocking_started: Arc::clone(&blocking_tool_started),
    });
    mock.runtime().register_tool(BlockingParallelEvidenceTool {
        started: blocking_tool_started,
    });

    let mut session = mock
        .runtime()
        .create_session("parallel-cancellation-observer", mock.model())
        .unwrap();
    let cancellation = CancellationToken::default();
    let cancellation_for_tap = cancellation.clone();
    let observed = Arc::new(Mutex::new(Vec::<AgentEvent>::new()));
    let observed_for_tap = Arc::clone(&observed);
    let _tap = session.register_agent_event_tap(move |event| {
        let completed_call = matches!(
            event,
            AgentEvent::ToolExecutionFinished {
                result: ContentBlock::ToolResult {
                    tool_use_id,
                    is_error: false,
                    ..
                },
            } if tool_use_id == "call-completed"
        );
        observed_for_tap
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(event.clone());
        if completed_call {
            // Cancellation happens only after the lossless tap retained the
            // completed sibling's event. The exact payload is asserted below;
            // the other tool remains pending while cancellation unwinds.
            cancellation_for_tap.cancel();
        }
    });

    let result = timeout(
        Duration::from_secs(5),
        session.append_turn_with_options(
            vec![ContentBlock::text("run the parallel evidence tools")],
            RunOptions {
                cancellation: Some(cancellation),
                ..RunOptions::default()
            },
        ),
    )
    .await
    .expect("parallel cancellation must not leave the sibling tool pending forever");

    assert!(matches!(result, Err(RuntimeError::Cancelled)));
    assert_eq!(
        post_execution_calls.load(Ordering::SeqCst),
        0,
        "the cancelled parallel batch never reaches the post-execution hook"
    );

    let events = observed
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let blocking_started_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                AgentEvent::ToolExecutionStarted { call }
                    if call.id == "call-blocking"
            )
        })
        .expect("the pending sibling must have started");
    let completed = events
        .iter()
        .position(|event| {
            matches!(
                event,
                AgentEvent::ToolExecutionFinished {
                    result: ContentBlock::ToolResult {
                        tool_use_id,
                        content: ToolResultContent::Structured(content),
                        is_error: false,
                    },
                } if tool_use_id == "call-completed" && content == &cancelled_parallel_result()
            )
        })
        .expect("the completed sibling's full result must be retained");
    let failed = events
        .iter()
        .position(|event| {
            matches!(
                event,
                AgentEvent::RunFailed { error } if error == "operation cancelled"
            )
        })
        .expect("cancellation must remain observable as RunFailed");

    assert!(blocking_started_index < completed);
    assert!(completed < failed);
    assert!(events.iter().all(|event| !matches!(
        event,
        AgentEvent::ToolExecutionFinished {
            result: ContentBlock::ToolResult { tool_use_id, .. },
        } if tool_use_id == "call-blocking"
    )));
    assert_eq!(
        failed,
        events.len() - 1,
        "RunFailed must terminate the tap sequence"
    );
}

#[tokio::test]
async fn cancellation_reaches_the_observer_as_run_failed() {
    let mock = MockRuntime::builder().text("not reached").build().unwrap();
    let mut session = mock
        .runtime()
        .create_session("cancelled-observer", mock.model())
        .unwrap();
    let observed = Arc::new(Mutex::new(Vec::<AgentEvent>::new()));
    let observed_for_tap = Arc::clone(&observed);
    let _tap = session.register_agent_event_tap(move |event| {
        observed_for_tap
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(event.clone());
    });
    let cancellation = CancellationToken::default();
    cancellation.cancel();

    let result = session
        .append_turn_with_options(
            vec![ContentBlock::text("cancel")],
            RunOptions {
                cancellation: Some(cancellation),
                ..RunOptions::default()
            },
        )
        .await;

    assert!(matches!(result, Err(RuntimeError::Cancelled)));
    let events = observed.lock().unwrap_or_else(|error| error.into_inner());
    assert!(matches!(events.first(), Some(AgentEvent::RunStarted)));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::RunFailed { error }) if error == "operation cancelled"
    ));
}

#[tokio::test]
async fn dropping_the_guard_unregisters_the_observer() {
    let mock = MockRuntime::builder()
        .text("first")
        .text("second")
        .build()
        .unwrap();
    let mut session = mock
        .runtime()
        .create_session("drop-observer", mock.model())
        .unwrap();
    let observed = Arc::new(AtomicUsize::new(0));
    let observed_for_tap = Arc::clone(&observed);
    let tap = session.register_agent_event_tap(move |_| {
        observed_for_tap.fetch_add(1, Ordering::SeqCst);
    });

    session
        .append_turn(vec![ContentBlock::text("first")])
        .await
        .unwrap();
    let after_first_turn = observed.load(Ordering::SeqCst);
    assert!(after_first_turn > 0);

    drop(tap);
    session
        .append_turn(vec![ContentBlock::text("second")])
        .await
        .unwrap();
    assert_eq!(observed.load(Ordering::SeqCst), after_first_turn);
}
