use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::{
    BuiltinProvider, ContentBlock, Message, QueueMode, Role, RoundContext, RoundDecision,
    RoundStrategy, Runtime, SteeringHandle,
    provider::{ContentBlockDelta, ContentBlockStart, ProviderError, ProviderEvent},
    runtime::{CancellationToken, RunOptions},
};

use super::support::{
    ScriptedProvider, StaticTool, controlled_stream, erroring_stream, model_info, text_stream,
    tool_use_stream,
};

#[tokio::test]
async fn live_steer_is_visible_in_the_next_provider_request() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let (first_stream, first_tx) = controlled_stream();
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![first_stream, text_stream(&model.id, "final")],
    );
    let provider_handle = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model.clone()).expect("spawn agent");
    let steering = agent.steering_handle();

    let drive = async {
        wait_for_request_count(&provider_handle, 1).await;
        steering.steer(vec![ContentBlock::text("focus on the API contract")]);
        send_text_response(&first_tx, &model.id, "draft");
        drop(first_tx);
    };
    let (result, ()) = tokio::join!(
        agent.run(vec![ContentBlock::text("start")], RunOptions::default()),
        drive
    );

    assert_eq!(result.expect("run succeeds").text(), "final");
    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 2);
    assert!(request_contains(
        &requests[1].messages,
        "focus on the API contract"
    ));
}

#[tokio::test]
async fn follow_up_waits_for_the_would_stop_boundary() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "call-1", "probe", r#"{"value":"x"}"#),
            text_stream(&model.id, "tool round complete"),
            text_stream(&model.id, "follow-up complete"),
        ],
    );
    let provider_handle = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("probe", "ok"))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");
    agent.follow_up(vec![ContentBlock::text("now produce the appendix")]);

    let result = agent
        .run(vec![ContentBlock::text("start")], RunOptions::default())
        .await
        .expect("run succeeds");

    assert_eq!(result.text(), "follow-up complete");
    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 3);
    assert!(!request_contains(
        &requests[1].messages,
        "now produce the appendix"
    ));
    assert!(request_contains(
        &requests[2].messages,
        "now produce the appendix"
    ));
}

#[tokio::test]
async fn queue_modes_drain_one_or_all_entries_per_boundary() {
    let one_at_a_time = run_with_queue_mode(QueueMode::OneAtATime).await;
    assert_eq!(one_at_a_time.len(), 3);
    assert!(request_contains(&one_at_a_time[1], "first steer"));
    assert!(!request_contains(&one_at_a_time[1], "second steer"));
    assert!(request_contains(&one_at_a_time[2], "second steer"));

    let all = run_with_queue_mode(QueueMode::All).await;
    assert_eq!(all.len(), 2);
    assert!(request_contains(&all[1], "first steer"));
    assert!(request_contains(&all[1], "second steer"));
}

#[test]
fn clear_methods_remove_only_their_pending_queue() {
    assert_eq!(QueueMode::default(), QueueMode::OneAtATime);

    let steering = SteeringHandle::default();
    steering.steer(vec![ContentBlock::text("steer")]);
    steering.follow_up(vec![ContentBlock::text("follow-up")]);

    steering.clear_steer();
    assert!(steering.has_pending(), "the follow-up remains queued");

    steering.steer(vec![ContentBlock::text("replacement steer")]);
    steering.clear_follow_up();
    assert!(
        steering.has_pending(),
        "the replacement steer remains queued"
    );

    steering.clear_steer();
    assert!(!steering.has_pending());
}

#[tokio::test]
async fn follow_up_all_mode_drains_every_entry_at_the_would_stop_boundary() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            text_stream(&model.id, "draft"),
            text_stream(&model.id, "final"),
        ],
    );
    let provider_handle = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");
    let steering = agent.steering_handle();
    steering.set_follow_up_mode(QueueMode::All);
    steering.follow_up(vec![ContentBlock::text("first follow-up")]);
    steering.follow_up(vec![ContentBlock::text("second follow-up")]);

    let result = agent
        .send(vec![ContentBlock::text("start")])
        .await
        .expect("run succeeds");

    assert_eq!(result.text(), "final");
    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 2);
    assert!(request_contains(&requests[1].messages, "first follow-up"));
    assert!(request_contains(&requests[1].messages, "second follow-up"));
    assert!(!steering.has_pending());
}

#[tokio::test]
async fn failed_run_requeues_steer_and_resume_reinjects_it() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            text_stream(&model.id, "first draft"),
            erroring_stream(
                Vec::new(),
                ProviderError::MalformedStream("failed after steer".to_string()),
            ),
            text_stream(&model.id, "retry draft"),
            text_stream(&model.id, "fixed"),
        ],
    );
    let provider_handle = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");
    let steering = agent.steering_handle();
    steering.steer(vec![ContentBlock::text("repair the draft")]);

    agent
        .run(vec![ContentBlock::text("start")], RunOptions::default())
        .await
        .expect_err("second request fails");
    assert!(steering.has_pending(), "failed run requeues the steer");

    let result = agent.resume().await.expect("resume succeeds");
    assert_eq!(result.text(), "fixed");
    assert!(!steering.has_pending());
    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 4);
    assert!(request_contains(&requests[1].messages, "repair the draft"));
    assert!(request_contains(&requests[3].messages, "repair the draft"));
}

#[tokio::test]
async fn failed_run_requeues_follow_up_and_resume_reinjects_it() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            text_stream(&model.id, "first draft"),
            erroring_stream(
                Vec::new(),
                ProviderError::MalformedStream("failed after follow-up".to_string()),
            ),
            text_stream(&model.id, "retry draft"),
            text_stream(&model.id, "fixed"),
        ],
    );
    let provider_handle = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");
    let steering = agent.steering_handle();
    steering.follow_up(vec![ContentBlock::text("append the required evidence")]);

    agent
        .run(vec![ContentBlock::text("start")], RunOptions::default())
        .await
        .expect_err("second request fails");
    assert!(steering.has_pending(), "failed run requeues the follow-up");

    let result = agent.resume().await.expect("resume succeeds");
    assert_eq!(result.text(), "fixed");
    assert!(!steering.has_pending());
    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 4);
    assert!(!request_contains(
        &requests[0].messages,
        "append the required evidence"
    ));
    assert!(request_contains(
        &requests[1].messages,
        "append the required evidence"
    ));
    assert!(!request_contains(
        &requests[2].messages,
        "append the required evidence"
    ));
    assert!(request_contains(
        &requests[3].messages,
        "append the required evidence"
    ));
    assert_eq!(
        agent
            .history()
            .iter()
            .filter(|message| message.text().contains("append the required evidence"))
            .count(),
        1
    );
}

#[tokio::test]
async fn steering_precedes_round_strategy_without_double_injection() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            text_stream(&model.id, "draft"),
            text_stream(&model.id, "steered"),
            text_stream(&model.id, "strategized"),
        ],
    );
    let provider_handle = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");
    agent.steer(vec![ContentBlock::text("queue correction")]);
    let strategy = Arc::new(InjectOnceStrategy::default());

    agent
        .run(
            vec![ContentBlock::text("start")],
            RunOptions::default().with_round_strategy(strategy.clone()),
        )
        .await
        .expect("run succeeds");

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 3);
    assert!(request_contains(&requests[1].messages, "queue correction"));
    assert!(!request_contains(
        &requests[1].messages,
        "strategy correction"
    ));
    assert!(request_contains(
        &requests[2].messages,
        "strategy correction"
    ));
    assert_eq!(strategy.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn run_queued_consumes_idle_steer_as_the_user_turn() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![text_stream(&model.id, "done")],
    );
    let provider_handle = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");
    agent.steer(vec![ContentBlock::text("queued while idle")]);

    let result = agent
        .run_queued(RunOptions::default())
        .await
        .expect("queued run succeeds");
    assert_eq!(result.text(), "done");
    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 1);
    assert!(request_contains(&requests[0].messages, "queued while idle"));
}

#[tokio::test]
async fn steering_is_isolated_between_agents_on_one_runtime() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            text_stream(&model.id, "agent-b done"),
            text_stream(&model.id, "agent-a draft"),
            text_stream(&model.id, "agent-a done"),
        ],
    );
    let provider_handle = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent_a = runtime.spawn("agent-a", model.clone()).expect("spawn a");
    let mut agent_b = runtime.spawn("agent-b", model).expect("spawn b");
    agent_a.steer(vec![ContentBlock::text("only agent a sees this")]);

    agent_b
        .send(vec![ContentBlock::text("run b")])
        .await
        .expect("run b");
    agent_a
        .send(vec![ContentBlock::text("run a")])
        .await
        .expect("run a");

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 3);
    assert!(!request_contains(
        &requests[0].messages,
        "only agent a sees this"
    ));
    assert!(request_contains(
        &requests[2].messages,
        "only agent a sees this"
    ));
}

#[tokio::test]
async fn graceful_stop_does_not_consume_an_unrequestable_steer() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let (first_stream, first_tx) = controlled_stream();
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![first_stream, text_stream(&model.id, "must not run")],
    );
    let provider_handle = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model.clone()).expect("spawn agent");
    let steering = agent.steering_handle();
    steering.steer(vec![ContentBlock::text("keep for later")]);
    let stop = CancellationToken::default();
    let stop_driver = stop.clone();

    let drive = async {
        wait_for_request_count(&provider_handle, 1).await;
        stop_driver.cancel();
        send_text_response(&first_tx, &model.id, "finished before steer");
        drop(first_tx);
    };
    let (result, ()) = tokio::join!(
        agent.run(
            vec![ContentBlock::text("start")],
            RunOptions {
                stop: Some(stop),
                ..RunOptions::default()
            }
        ),
        drive
    );

    assert_eq!(
        result.expect("graceful stop succeeds").text(),
        "finished before steer"
    );
    assert!(steering.has_pending());
    assert_eq!(provider_handle.recorded_requests().await.len(), 1);
}

async fn run_with_queue_mode(mode: QueueMode) -> Vec<Vec<Message>> {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let scripts = match mode {
        QueueMode::OneAtATime => vec![
            text_stream(&model.id, "first"),
            text_stream(&model.id, "second"),
            text_stream(&model.id, "third"),
        ],
        QueueMode::All => vec![
            text_stream(&model.id, "first"),
            text_stream(&model.id, "second"),
        ],
    };
    let provider = ScriptedProvider::new(BuiltinProvider::Anthropic, vec![model.clone()], scripts);
    let provider_handle = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");
    let steering = agent.steering_handle();
    steering.set_steer_mode(mode);
    steering.steer(vec![ContentBlock::text("first steer")]);
    steering.steer(vec![ContentBlock::text("second steer")]);

    agent
        .send(vec![ContentBlock::text("start")])
        .await
        .expect("run");
    provider_handle
        .recorded_requests()
        .await
        .into_iter()
        .map(|request| request.messages.to_vec())
        .collect()
}

#[derive(Default)]
struct InjectOnceStrategy {
    calls: AtomicUsize,
}

#[async_trait]
impl RoundStrategy for InjectOnceStrategy {
    async fn on_round(&self, _ctx: RoundContext<'_>) -> RoundDecision {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            RoundDecision::inject(vec![ContentBlock::text("strategy correction")])
        } else {
            RoundDecision::stop()
        }
    }
}

fn request_contains(messages: &[Message], needle: &str) -> bool {
    messages
        .iter()
        .any(|message| message.text().contains(needle))
}

async fn wait_for_request_count(provider: &ScriptedProvider, expected: usize) {
    loop {
        if provider.recorded_requests().await.len() >= expected {
            return;
        }
        tokio::task::yield_now().await;
    }
}

fn send_text_response(
    tx: &mpsc::UnboundedSender<Result<ProviderEvent, ProviderError>>,
    model: &str,
    text: &str,
) {
    let events = [
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
        ProviderEvent::MessageStopped,
    ];
    for event in events {
        tx.send(Ok(event)).expect("stream receiver remains alive");
    }
}
