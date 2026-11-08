use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use crate::{
    BuiltinProvider, ContentBlock, Role,
    agent::AgentEvent,
    provider::{ContentBlockDelta, ContentBlockStart, ProviderEvent},
    runtime::{
        AuditStore, CancellationToken, RunOptions, Runtime, RuntimeError, RuntimeHook,
        RuntimeHookEvent, VolatileRuntimeStore,
    },
};

use super::support::{ScriptedProvider, StreamScript, controlled_stream, model_info};

const CANCEL_AFTER: Duration = Duration::from_millis(40);
const CANCEL_WATCHDOG: Duration = Duration::from_millis(200);
// Deadline tests use wall-clock `SystemTime`, rather than Tokio's virtual
// clock. Leave the test harness several seconds to first poll the run when
// hundreds of sibling tests are running in parallel; the runtime still
// returns promptly once the provider stream has been accepted.
const DEADLINE_AFTER: Duration = Duration::from_secs(5);
const DEADLINE_WATCHDOG: Duration = Duration::from_secs(15);

fn input() -> Vec<ContentBlock> {
    vec![ContentBlock::text("hello")]
}

#[tokio::test(start_paused = true)]
async fn cancellation_stops_a_provider_call_before_it_opens_the_model_stream() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![StreamScript::Pending],
    );
    let request_log = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");
    let cancellation = CancellationToken::default();
    let canceller = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(CANCEL_AFTER).await;
        canceller.cancel();
    });

    let started = tokio::time::Instant::now();
    let error = tokio::time::timeout(
        CANCEL_WATCHDOG,
        agent.run(
            input(),
            RunOptions {
                cancellation: Some(cancellation),
                ..RunOptions::default()
            },
        ),
    )
    .await
    .expect("cancellation must bound the provider stream call")
    .expect_err("the cancelled run must fail");

    assert!(matches!(error, RuntimeError::Cancelled));
    assert!(
        started.elapsed() < CANCEL_WATCHDOG,
        "the run must observe cancellation before the watchdog"
    );
    assert_eq!(request_log.recorded_requests().await.len(), 1);
}

#[tokio::test]
async fn deadline_stops_a_provider_call_before_it_opens_the_model_stream() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![StreamScript::Pending],
    );
    let request_log = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    let error = tokio::time::timeout(
        DEADLINE_WATCHDOG,
        agent.run(
            input(),
            RunOptions {
                deadline: Some(SystemTime::now() + DEADLINE_AFTER),
                ..RunOptions::default()
            },
        ),
    )
    .await
    .expect("the run deadline must bound the provider stream call")
    .expect_err("the expired run must fail");

    assert!(matches!(error, RuntimeError::DeadlineExceeded));
    assert_eq!(request_log.recorded_requests().await.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn cancellation_stops_waiting_for_the_first_model_event() {
    assert_stream_cancellation(false).await;
}

#[tokio::test(start_paused = true)]
async fn cancellation_stops_waiting_after_a_partial_model_response() {
    assert_stream_cancellation(true).await;
}

async fn assert_stream_cancellation(send_partial_response: bool) {
    let cancellation = CancellationToken::default();
    let canceller = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(CANCEL_AFTER).await;
        canceller.cancel();
    });

    let error = stopped_model_stream(
        RunOptions {
            cancellation: Some(cancellation),
            ..RunOptions::default()
        },
        CANCEL_WATCHDOG,
        send_partial_response,
    )
    .await;
    assert!(matches!(error, RuntimeError::Cancelled));
}

#[tokio::test]
async fn deadline_stops_waiting_for_the_first_model_event() {
    assert_stream_deadline(false).await;
}

#[tokio::test]
async fn deadline_stops_waiting_after_a_partial_model_response() {
    assert_stream_deadline(true).await;
}

async fn assert_stream_deadline(send_partial_response: bool) {
    let error = stopped_model_stream(
        RunOptions {
            deadline: Some(SystemTime::now() + DEADLINE_AFTER),
            ..RunOptions::default()
        },
        DEADLINE_WATCHDOG,
        send_partial_response,
    )
    .await;
    assert!(matches!(error, RuntimeError::DeadlineExceeded));
}

async fn stopped_model_stream(
    options: RunOptions,
    watchdog: Duration,
    send_partial_response: bool,
) -> RuntimeError {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let (stream, stream_tx) = controlled_stream();
    if send_partial_response {
        for event in [
            ProviderEvent::MessageStarted {
                id: "partial".to_string(),
                model: model.id.clone(),
                role: Role::Assistant,
            },
            ProviderEvent::ContentBlockStarted {
                index: 0,
                kind: ContentBlockStart::Text,
            },
            ProviderEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text("partial".to_string()),
            },
        ] {
            stream_tx.send(Ok(event)).expect("queue partial response");
        }
    }

    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![stream],
    );
    let request_log = provider.clone();
    let response_hooks = ResponseHooks::default();
    let runtime = Runtime::empty_builder()
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .with_hook(response_hooks.clone())
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");
    let mut events = agent.subscribe_events();

    let error = tokio::time::timeout(watchdog, agent.run(input(), options))
        .await
        .expect("run bounds must stop a stalled model stream before the watchdog")
        .expect_err("the stalled run must fail");

    assert!(
        stream_tx.is_closed(),
        "the provider receiver must be released"
    );
    assert_eq!(request_log.recorded_requests().await.len(), 1);
    assert_eq!(
        std::iter::from_fn(|| events.try_recv().ok())
            .any(|event| matches!(event, AgentEvent::TextDelta { .. })),
        send_partial_response,
    );
    assert!(
        agent.history().is_empty(),
        "the partial turn must roll back"
    );

    let response_hooks = response_hooks.0.lock().expect("response hooks poisoned");
    assert!(matches!(
        response_hooks.as_slice(),
        [RuntimeHookEvent::ModelResponseFinished {
            attempt: 1,
            success: false,
            error: Some(reported),
            stop_reason: None,
            usage: None,
            ..
        }] if reported == &error.to_string()
    ));
    error
}

#[derive(Clone, Default)]
struct ResponseHooks(Arc<Mutex<Vec<RuntimeHookEvent>>>);

impl RuntimeHook for ResponseHooks {
    fn on_event(
        &self,
        _store: &dyn AuditStore,
        event: &RuntimeHookEvent,
    ) -> Result<(), RuntimeError> {
        if matches!(event, RuntimeHookEvent::ModelResponseFinished { .. }) {
            self.0
                .lock()
                .expect("response hooks poisoned")
                .push(event.clone());
        }
        Ok(())
    }
}
