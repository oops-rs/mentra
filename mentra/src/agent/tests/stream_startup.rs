use std::time::{Duration, SystemTime};

use crate::{
    BuiltinProvider, ContentBlock,
    runtime::{CancellationToken, RunOptions, Runtime, RuntimeError},
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
    let model = model_info("model", BuiltinProvider::Anthropic);
    let (stream, stream_tx) = controlled_stream();
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![stream],
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
    .expect("cancellation must bound the wait for the first model event")
    .expect_err("the cancelled run must fail");

    assert!(matches!(error, RuntimeError::Cancelled));
    assert!(
        started.elapsed() < CANCEL_WATCHDOG,
        "the run must observe cancellation before the watchdog"
    );
    assert_eq!(request_log.recorded_requests().await.len(), 1);
    drop(stream_tx);
}

#[tokio::test]
async fn deadline_stops_waiting_for_the_first_model_event() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let (stream, stream_tx) = controlled_stream();
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![stream],
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
    .expect("the run deadline must bound the wait for the first model event")
    .expect_err("the expired run must fail");

    assert!(matches!(error, RuntimeError::DeadlineExceeded));
    assert_eq!(request_log.recorded_requests().await.len(), 1);
    drop(stream_tx);
}
