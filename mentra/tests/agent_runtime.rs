use std::{
    collections::VecDeque,
    fs,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use mentra::runtime::{RunOptions, SqliteRuntimeStore};
use mentra::{
    AgentConfig, BuiltinProvider, ContentBlock, Message, Role, Runtime,
    agent::{AgentEvent, AgentStatus, RoundContext, RoundDecision, RoundStrategy},
    error::RuntimeError,
    provider::{
        ContentBlockDelta, ContentBlockStart, ModelInfo, Provider, ProviderDescriptor,
        ProviderError, ProviderEvent, ProviderEventStream, ProviderId, Request,
    },
};
use reqwest::StatusCode;
use tokio::sync::{Mutex, broadcast, mpsc};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

enum StreamScript {
    Buffered(Vec<Result<ProviderEvent, ProviderError>>),
    Fail(ProviderError),
}

#[derive(Clone)]
struct ScriptedProvider {
    kind: ProviderId,
    models: Vec<ModelInfo>,
    scripts: std::sync::Arc<Mutex<VecDeque<StreamScript>>>,
    requests: std::sync::Arc<Mutex<Vec<Request<'static>>>>,
}

impl ScriptedProvider {
    fn new(
        kind: impl Into<ProviderId>,
        models: Vec<ModelInfo>,
        scripts: Vec<StreamScript>,
    ) -> Self {
        Self {
            kind: kind.into(),
            models,
            scripts: std::sync::Arc::new(Mutex::new(VecDeque::from(scripts))),
            requests: std::sync::Arc::new(Mutex::new(Vec::new())),
        }
    }

    async fn recorded_requests(&self) -> Vec<Request<'static>> {
        self.requests.lock().await.clone()
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(self.kind.clone())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(self.models.clone())
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        self.requests.lock().await.push(request.into_owned());
        match self.scripts.lock().await.pop_front() {
            Some(StreamScript::Buffered(items)) => {
                let (tx, rx) = mpsc::unbounded_channel();
                for item in items {
                    tx.send(item)
                        .expect("test stream receiver dropped unexpectedly");
                }
                Ok(rx)
            }
            Some(StreamScript::Fail(error)) => Err(error),
            None => panic!("no scripted stream available"),
        }
    }
}

#[tokio::test]
async fn send_streamed_text_turn_emits_events_and_commits_history() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![text_stream(&model.id, "Hello")],
    );

    let runtime = test_runtime(provider);
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                system: Some("system prompt".to_string()),
                ..AgentConfig::default()
            },
        )
        .unwrap();
    let mut events = agent.subscribe_events();

    let message = agent.send(vec![ContentBlock::text("hi")]).await.unwrap();

    assert_eq!(message, Message::assistant(ContentBlock::text("Hello")));
    assert_eq!(agent.name(), "agent");
    assert_eq!(agent.model(), "model");
    assert_eq!(agent.history().len(), 2);
    assert_eq!(agent.config().system.as_deref(), Some("system prompt"));
    assert_eq!(
        agent.last_message(),
        Some(&Message::assistant(ContentBlock::text("Hello")))
    );

    let events = collect_events(&mut events);
    assert!(events.contains(&AgentEvent::RunStarted));
    assert!(events.contains(&AgentEvent::TextDelta {
        delta: "Hello".to_string(),
        full_text: "Hello".to_string(),
    }));
    assert!(matches!(events.last(), Some(AgentEvent::RunFinished)));

    let snapshot = agent.watch_snapshot();
    assert_eq!(snapshot.borrow().status, AgentStatus::Finished);
    assert_eq!(snapshot.borrow().history_len, 2);
    assert!(snapshot.borrow().current_text.is_empty());
    assert!(snapshot.borrow().pending_tool_uses.is_empty());
}

#[tokio::test]
async fn send_failure_rolls_history_back_and_emits_run_failed() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            text_stream(&model.id, "ok"),
            erroring_stream(
                vec![ProviderEvent::MessageStarted {
                    id: "msg-2".to_string(),
                    model: model.id.clone(),
                    role: Role::Assistant,
                }],
                ProviderError::MalformedStream("boom".to_string()),
            ),
        ],
    );

    let runtime = test_runtime(provider);
    let mut agent = runtime.spawn("agent", model).unwrap();
    agent.send(vec![ContentBlock::text("first")]).await.unwrap();
    let baseline = agent.history().to_vec();
    let mut events = agent.subscribe_events();

    let result = agent.send(vec![ContentBlock::text("second")]).await;
    assert!(result.is_err());
    assert_eq!(agent.history(), baseline.as_slice());

    let events = collect_events(&mut events);
    assert!(matches!(events.last(), Some(AgentEvent::RunFailed { .. })));
}

#[tokio::test]
async fn send_retries_transient_provider_error_before_streaming() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            failed_request(ProviderError::Http {
                status: StatusCode::SERVICE_UNAVAILABLE,
                body: "offline".to_string(),
            }),
            text_stream(&model.id, "recovered"),
        ],
    );
    let provider_handle = provider.clone();

    let runtime = test_runtime(provider);
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    let message = agent
        .send(vec![ContentBlock::text("hello")])
        .await
        .expect("send should retry");

    assert_eq!(message.text(), "recovered");
    assert_eq!(provider_handle.recorded_requests().await.len(), 2);
    assert_eq!(
        agent.last_message(),
        Some(&Message::assistant(ContentBlock::text("recovered")))
    );
}

/// Counters a [`RoundStrategy`] observed at the most recent round boundary,
/// captured by [`CountingStrategy`].
#[derive(Clone, Copy, Default)]
struct RoundCounters {
    rounds_completed: usize,
    model_requests: usize,
    transport_retries: usize,
}

/// A [`RoundStrategy`] that records [`RoundContext`]'s counters at each boundary
/// it observes, always proceeding.
struct CountingStrategy {
    last: Mutex<Option<RoundCounters>>,
}

impl CountingStrategy {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            last: Mutex::new(None),
        })
    }

    async fn last_counters(&self) -> RoundCounters {
        self.last.lock().await.expect("strategy observed a round")
    }
}

#[async_trait]
impl RoundStrategy for CountingStrategy {
    async fn on_round(&self, ctx: RoundContext<'_>) -> RoundDecision {
        *self.last.lock().await = Some(RoundCounters {
            rounds_completed: ctx.rounds_completed(),
            model_requests: ctx.model_requests(),
            transport_retries: ctx.transport_retries(),
        });
        RoundDecision::proceed()
    }
}

#[tokio::test]
async fn retry_and_round_counters_are_reported_distinctly() {
    // A connection-open retry, then a success: `model_requests` (today's
    // request-including-retries counter) must stay at 2, `rounds_completed` (the
    // logical-round counter) must land at 1 — the round that needed a retry still
    // counts as exactly one completed round — and the new `transport_retries`
    // counter must isolate the one retry, distinct from both.
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            failed_request(ProviderError::Http {
                status: StatusCode::SERVICE_UNAVAILABLE,
                body: "offline".to_string(),
            }),
            text_stream(&model.id, "recovered"),
        ],
    );
    let provider_handle = provider.clone();
    let runtime = test_runtime(provider);
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    let strategy = CountingStrategy::new();
    let message = agent
        .run(
            vec![ContentBlock::text("hello")],
            RunOptions::default().with_round_strategy(strategy.clone()),
        )
        .await
        .expect("run should retry then succeed");

    assert_eq!(message.text(), "recovered");
    assert_eq!(provider_handle.recorded_requests().await.len(), 2);

    let counters = strategy.last_counters().await;
    assert_eq!(counters.rounds_completed, 1, "one logical round completed");
    assert_eq!(
        counters.model_requests, 2,
        "model_requests keeps today's semantics: it counts the retry and the success"
    );
    assert_eq!(
        counters.transport_retries, 1,
        "exactly one transient retry, isolated from rounds_completed and reported distinctly"
    );
}

#[tokio::test]
async fn resume_replays_last_failed_turn() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            erroring_stream(
                vec![ProviderEvent::MessageStarted {
                    id: "msg-1".to_string(),
                    model: model.id.clone(),
                    role: Role::Assistant,
                }],
                ProviderError::MalformedStream("boom".to_string()),
            ),
            text_stream(&model.id, "done"),
        ],
    );

    let runtime = test_runtime(provider);
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    let error = agent
        .send(vec![ContentBlock::text("retry me")])
        .await
        .expect_err("first send should fail");
    assert!(matches!(error, RuntimeError::FailedToStreamResponse(_)));
    assert!(agent.history().is_empty());

    let resumed = agent
        .resume()
        .await
        .expect("resume should replay user turn");

    assert_eq!(resumed.text(), "done");
    assert_eq!(agent.history().len(), 2);
    assert_eq!(
        agent.history()[0],
        Message::user(ContentBlock::text("retry me"))
    );
    assert_eq!(
        agent.history()[1],
        Message::assistant(ContentBlock::text("done"))
    );

    let error = agent
        .resume()
        .await
        .expect_err("successful run clears resume state");
    assert!(matches!(error, RuntimeError::NoResumableTurn));
}

fn collect_events(receiver: &mut broadcast::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        events.push(event);
    }
    events
}

fn test_runtime(provider: ScriptedProvider) -> Runtime {
    Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_store(temp_store("agent-runtime"))
        .build()
        .expect("build runtime")
}

fn temp_store(label: &str) -> SqliteRuntimeStore {
    let unique = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "mentra-agent-runtime-{label}-{timestamp}-{unique}.sqlite"
    ));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create temp dir");
    }
    SqliteRuntimeStore::new(path)
}

fn model_info(id: &str, provider: impl Into<ProviderId>) -> ModelInfo {
    ModelInfo::new(id, provider)
}

fn buffered_stream(events: Vec<ProviderEvent>) -> StreamScript {
    StreamScript::Buffered(events.into_iter().map(Ok).collect())
}

fn erroring_stream(mut events: Vec<ProviderEvent>, error: ProviderError) -> StreamScript {
    let mut items = events.drain(..).map(Ok).collect::<Vec<_>>();
    items.push(Err(error));
    StreamScript::Buffered(items)
}

fn failed_request(error: ProviderError) -> StreamScript {
    StreamScript::Fail(error)
}

fn text_stream(model: &str, text: &str) -> StreamScript {
    buffered_stream(vec![
        ProviderEvent::MessageStarted {
            id: "msg-text".to_string(),
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
        ProviderEvent::MessageStopped,
    ])
}
