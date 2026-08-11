//! Tests for [`Session::append_turn_to_output`]: the typed value it hands
//! back, what the session stream says around a typed turn, and what a typed
//! turn that fails leaves behind.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    BuiltinProvider, ContentBlock, ModelInfo, Provider, ProviderDescriptor, ProviderError,
    ProviderEventStream, Request, Role, Runtime, TerminalOutputSpec, ToolChoice,
    provider::Response,
    provider_event_stream_from_response,
    runtime::RunOptions,
    session::{Session, SessionEvent, SessionStatus},
};

/// What the model writes when it declines the forced tool and answers in prose.
const PLAIN_ANSWER: &str = "plain answer";

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct Report {
    answer: u64,
    evidence: Vec<String>,
}

/// Plays the model for a terminal-output run: when the request forces one
/// tool, it calls exactly that tool, so a test never has to know the tool name
/// `run_to_output` generates for the run. Without a forced choice — an
/// ordinary turn on the same session — it answers in prose.
#[derive(Clone)]
struct ForcedToolProvider {
    model: ModelInfo,
    /// Prose the model writes alongside the terminal call.
    preface: Option<String>,
    /// Input the model sends to the forced tool. `None` makes it ignore the
    /// forced choice and answer in prose instead.
    payload: Option<Value>,
    calls: Arc<AtomicUsize>,
}

impl ForcedToolProvider {
    fn new(payload: Option<Value>) -> Self {
        Self {
            model: ModelInfo::new("typed-output-model", BuiltinProvider::Anthropic),
            preface: None,
            payload,
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn answering(payload: Value) -> Self {
        Self::new(Some(payload))
    }

    fn ignoring_the_forced_tool() -> Self {
        Self::new(None)
    }

    fn with_preface(mut self, preface: &str) -> Self {
        self.preface = Some(preface.to_string());
        self
    }
}

#[async_trait]
impl Provider for ForcedToolProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(self.model.provider.clone())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![self.model.clone()])
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let forced = match request.tool_choice.clone() {
            Some(ToolChoice::Tool { name }) => Some(name),
            _ => None,
        };

        let (content, stop_reason) = match (forced, self.payload.clone()) {
            (Some(name), Some(payload)) => {
                let mut blocks = Vec::new();
                if let Some(preface) = &self.preface {
                    blocks.push(ContentBlock::text(preface.clone()));
                }
                blocks.push(ContentBlock::ToolUse {
                    id: format!("terminal-call-{call}"),
                    name,
                    input: payload,
                });
                (blocks, Some("tool_use".to_string()))
            }
            _ => (vec![ContentBlock::text(PLAIN_ANSWER)], None),
        };

        Ok(provider_event_stream_from_response(Response {
            id: format!("message-{call}-{}", unique_suffix()),
            model: self.model.id.clone(),
            role: Role::Assistant,
            content,
            stop_reason,
            usage: None,
        }))
    }
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

/// The runtime is returned alongside the session so it outlives the turn and
/// keeps the agent's lease held.
fn session_for(provider: ForcedToolProvider) -> (Runtime, Session) {
    let model = provider.model.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let session = runtime
        .create_session("typed-output", model)
        .expect("create session");
    (runtime, session)
}

fn report_spec() -> TerminalOutputSpec {
    TerminalOutputSpec::new(
        "finish-report",
        "Return the final report",
        json!({
            "type": "object",
            "properties": {
                "answer": { "type": "integer" },
                "evidence": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["answer", "evidence"]
        }),
    )
}

fn drain(rx: &mut crate::session::SessionEventReceiver) -> Vec<SessionEvent> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

fn position(
    events: &[SessionEvent],
    label: &str,
    predicate: impl Fn(&SessionEvent) -> bool,
) -> usize {
    events
        .iter()
        .position(predicate)
        .unwrap_or_else(|| panic!("expected a {label} event, got: {events:?}"))
}

#[tokio::test]
async fn a_typed_turn_returns_the_value_and_counts_as_a_turn() {
    let (_runtime, mut session) = session_for(ForcedToolProvider::answering(
        json!({ "answer": 42, "evidence": ["a", "b"] }),
    ));

    let output = session
        .append_turn_to_output::<Report>(
            vec![ContentBlock::text("produce the report")],
            RunOptions::default(),
            report_spec(),
        )
        .await
        .expect("a typed turn succeeds");

    assert_eq!(
        output.value,
        Report {
            answer: 42,
            evidence: vec!["a".to_string(), "b".to_string()],
        }
    );
    // The turn ends on the tool-result message, not on assistant text — the
    // asymmetry the session's terminal event has to account for.
    assert_eq!(output.message.role, Role::User);
    assert!(matches!(
        output.message.content.as_slice(),
        [ContentBlock::ToolResult { tool_use_id, .. }] if tool_use_id == "terminal-call-0"
    ));

    assert_eq!(session.metadata().turn_count, 1);
    assert_eq!(session.metadata().status, SessionStatus::Idle);
    assert!(
        !session.replay().items().is_empty(),
        "the typed turn is committed to the transcript"
    );
}

#[tokio::test]
async fn a_typed_turn_completes_with_the_model_prose_after_the_terminal_tool_events() {
    let (_runtime, mut session) = session_for(
        ForcedToolProvider::answering(json!({ "answer": 7, "evidence": [] }))
            .with_preface("here is the report"),
    );
    let mut rx = session.subscribe();

    session
        .append_turn_to_output::<Report>(
            vec![ContentBlock::text("produce the report")],
            RunOptions::default(),
            report_spec(),
        )
        .await
        .expect("a typed turn succeeds");

    let events = drain(&mut rx);

    let user = position(
        &events,
        "UserMessage",
        |event| matches!(event, SessionEvent::UserMessage { text } if text == "produce the report"),
    );
    let queued = position(&events, "ToolQueued", |event| {
        matches!(event, SessionEvent::ToolQueued { tool_name, .. }
            if tool_name.starts_with("mentra_terminal_"))
    });
    let started = position(&events, "ToolStarted", |event| {
        matches!(event, SessionEvent::ToolStarted { .. })
    });
    let completed = position(&events, "ToolCompleted", |event| {
        matches!(
            event,
            SessionEvent::ToolCompleted {
                is_error: false,
                ..
            }
        )
    });
    let done = position(&events, "AssistantMessageCompleted", |event| {
        matches!(event, SessionEvent::AssistantMessageCompleted { .. })
    });

    assert!(
        user < queued && queued < started && started < completed && completed < done,
        "a typed turn runs user -> terminal tool -> completion, got: {events:?}"
    );

    // The completion carries what the model wrote, matching the deltas that
    // were already streamed — not the typed payload.
    assert!(
        matches!(&events[done], SessionEvent::AssistantMessageCompleted { text }
            if text == "here is the report"),
        "got: {:?}",
        events[done]
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, SessionEvent::AssistantMessageCompleted { .. }))
            .count(),
        1,
        "one completion per turn, as for any other turn"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            SessionEvent::AssistantTokenDelta { full_text, .. } if full_text == "here is the report"
        )),
        "the completion agrees with the streamed deltas, got: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, SessionEvent::Error { .. })),
        "a successful typed turn reports no error, got: {events:?}"
    );

    // The payload is on the stream through the terminal tool's own events,
    // which is why the completion does not repeat it as prose.
    let SessionEvent::ToolQueued { input_json, .. } = &events[queued] else {
        panic!("expected ToolQueued at {queued}");
    };
    assert_eq!(
        serde_json::from_str::<Value>(input_json).expect("the queued input is JSON"),
        json!({ "answer": 7, "evidence": [] })
    );
}

#[tokio::test]
async fn a_typed_turn_without_model_prose_completes_with_empty_text() {
    let (_runtime, mut session) = session_for(ForcedToolProvider::answering(
        json!({ "answer": 1, "evidence": [] }),
    ));
    let mut rx = session.subscribe();

    session
        .append_turn_to_output::<Report>(
            vec![ContentBlock::text("produce the report")],
            RunOptions::default(),
            report_spec(),
        )
        .await
        .expect("a typed turn succeeds");

    let events = drain(&mut rx);
    let done = position(&events, "AssistantMessageCompleted", |event| {
        matches!(event, SessionEvent::AssistantMessageCompleted { .. })
    });
    assert!(
        matches!(&events[done], SessionEvent::AssistantMessageCompleted { text } if text.is_empty()),
        "a model that writes only the terminal call completes with no prose, got: {:?}",
        events[done]
    );
}

#[tokio::test]
async fn a_value_that_does_not_match_the_type_fails_the_turn_and_the_session_recovers() {
    let (_runtime, mut session) = session_for(ForcedToolProvider::answering(
        json!({ "answer": "forty-two", "evidence": [] }),
    ));
    let mut rx = session.subscribe();

    let error = session
        .append_turn_to_output::<Report>(
            vec![ContentBlock::text("produce the report")],
            RunOptions::default(),
            report_spec(),
        )
        .await
        .expect_err("a value that is not a Report must fail the turn");

    assert!(
        error
            .to_string()
            .contains("did not match the requested type"),
        "got: {error}"
    );
    assert!(
        matches!(session.metadata().status, SessionStatus::Failed(_)),
        "a failed typed turn leaves the session Failed, like any failed turn"
    );
    assert_eq!(
        session.metadata().turn_count,
        0,
        "a failed turn does not move the counter"
    );

    let events = drain(&mut rx);
    assert!(
        events.iter().any(|event| matches!(
            event,
            SessionEvent::Error {
                recoverable: false,
                ..
            }
        )),
        "expected a terminal Error event, got: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, SessionEvent::AssistantMessageCompleted { .. })),
        "a failed turn emits no completion, got: {events:?}"
    );

    // Same as a failed `append_turn`: the session takes the next turn.
    let recovered = session
        .append_turn(vec![ContentBlock::text("try again")])
        .await
        .expect("the session accepts a turn after a failed typed turn");
    assert_eq!(recovered.text(), PLAIN_ANSWER);
    assert_eq!(session.metadata().status, SessionStatus::Idle);
    assert_eq!(session.metadata().turn_count, 1);
}

#[tokio::test]
async fn a_run_that_never_calls_the_terminal_tool_fails_the_turn() {
    let (_runtime, mut session) = session_for(ForcedToolProvider::ignoring_the_forced_tool());
    let mut rx = session.subscribe();

    let error = session
        .append_turn_to_output::<Report>(
            vec![ContentBlock::text("produce the report")],
            RunOptions::default(),
            report_spec(),
        )
        .await
        .expect_err("a run without the terminal call has no typed value to return");

    assert!(
        error
            .to_string()
            .contains("without invoking the expected terminal tool"),
        "got: {error}"
    );
    assert!(matches!(
        session.metadata().status,
        SessionStatus::Failed(_)
    ));
    assert_eq!(session.metadata().turn_count, 0);

    let events = drain(&mut rx);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::Error { .. })),
        "expected an Error event, got: {events:?}"
    );
}
