use std::{
    future::pending,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::{sync::Notify, time::timeout};

use crate::{
    Agent, AgentConfig, BuiltinProvider, ContentBlock, Runtime, ToolAudience,
    agent::{AgentEvent, ToolProfile},
    runtime::{
        CancellationToken, EarlyEnd, HookDecision, PostExecutionContext, PostExecutionHook,
        PreExecutionContext, PreExecutionHook, ResultDecision, RunOptions, RuntimeError,
    },
    tool::{
        ParallelToolContext, ProviderToolSpec, ReviewedToolResult, ToolAuthorizationDecision,
        ToolAuthorizationRequest, ToolAuthorizer, ToolContext, ToolDefinition,
        ToolExecutionCategory, ToolExecutor, ToolOutput, ToolResultContent, ToolSpec,
    },
    transcript::{AgentTranscript, TranscriptKind},
};

use super::{
    runtime_resume::{clear_leases, temp_store},
    runtime_tools::multi_tool_use_stream,
    support::{
        ScriptedProvider, StopTrippingTool, StreamScript, model_info, text_stream, tool_use_stream,
    },
};

const MODEL: &str = "model";
const SCRIPT_CALL: &str = "script-call";

#[derive(Default)]
struct Observed {
    offered: Arc<[ProviderToolSpec]>,
    results: Vec<ReviewedToolResult>,
    catches: usize,
}

struct Composer {
    name: &'static str,
    steps: Vec<(&'static str, Option<Value>)>,
    observed: Arc<Mutex<Observed>>,
    after_discovery: Option<(Arc<Notify>, Arc<Notify>)>,
}

impl Composer {
    fn new(name: &'static str, steps: Vec<(&'static str, Option<Value>)>) -> Self {
        Self {
            name,
            steps,
            observed: Arc::default(),
            after_discovery: None,
        }
    }
}

impl ToolDefinition for Composer {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder(self.name).build()
    }
}

#[async_trait]
impl ToolExecutor for Composer {
    async fn execute_mut_output(
        &self,
        mut ctx: ToolContext<'_>,
        _input: Value,
    ) -> Result<ToolOutput, String> {
        self.observed.lock().unwrap().offered = ctx.visible_tool_specs();
        if let Some((ready, go)) = &self.after_discovery {
            ready.notify_one();
            go.notified().await;
        }
        let mut previous = Value::Null;
        for (name, input) in &self.steps {
            let input = input.clone().unwrap_or_else(|| previous.clone());
            let result = ctx.invoke_child(name, input).await;
            previous = match &result.content {
                ToolResultContent::Text(text) => Value::String(text.clone()),
                ToolResultContent::Structured(value) => value.clone(),
            };
            // The evaluator can handle an ordinary tool error and yield
            // another call. Stop and terminal control must never reach here.
            let mut observed = self.observed.lock().unwrap();
            observed.catches += usize::from(result.is_error);
            observed.results.push(result);
        }
        Ok(ToolOutput::text("selected result"))
    }
}

struct Leaf {
    spec: ToolSpec,
    output: ToolOutput,
    inputs: Arc<Mutex<Vec<Value>>>,
}

impl Leaf {
    fn new(name: &'static str, output: ToolOutput) -> Self {
        Self {
            spec: ToolSpec::builder(name)
                .execution_category(ToolExecutionCategory::ReadOnlyParallel)
                .build(),
            output,
            inputs: Arc::default(),
        }
    }
}

impl ToolDefinition for Leaf {
    fn descriptor(&self) -> ToolSpec {
        self.spec.clone()
    }
}

#[async_trait]
impl ToolExecutor for Leaf {
    async fn execute_output(
        &self,
        _ctx: ParallelToolContext,
        input: Value,
    ) -> Result<ToolOutput, String> {
        self.inputs.lock().unwrap().push(input);
        Ok(self.output.clone())
    }

    async fn execute_mut_output(
        &self,
        _ctx: ToolContext<'_>,
        input: Value,
    ) -> Result<ToolOutput, String> {
        assert!(self.spec.terminal, "ordinary leaves use the parallel lane");
        self.inputs.lock().unwrap().push(input);
        Ok(self.output.clone())
    }
}

struct Rewrite;

#[async_trait]
impl PreExecutionHook for Rewrite {
    async fn pre_tool_execution(
        &self,
        ctx: &PreExecutionContext,
    ) -> Result<HookDecision, RuntimeError> {
        if ctx.tool_name == "consume"
            && serde_json::from_str::<Value>(&ctx.input_json).unwrap() == json!({"id": "reviewed"})
        {
            return Ok(HookDecision::Modify {
                input_json: json!({"id": "admitted"}).to_string(),
                reason: Some("test input rewrite".into()),
            });
        }
        Ok(HookDecision::Allow)
    }
}

#[async_trait]
impl PostExecutionHook for Rewrite {
    async fn post_tool_execution(
        &self,
        ctx: &PostExecutionContext,
    ) -> Result<ResultDecision, RuntimeError> {
        if ctx.tool_name == "lookup" {
            return Ok(ResultDecision::Replace {
                content: ToolResultContent::Structured(json!({"id": "reviewed"})),
                is_error: false,
            });
        }
        Ok(ResultDecision::Keep)
    }
}

#[derive(Clone, Default)]
struct Authorizer(Arc<Mutex<Vec<ToolAuthorizationRequest>>>);

#[async_trait]
impl ToolAuthorizer for Authorizer {
    async fn authorize(
        &self,
        request: &ToolAuthorizationRequest,
    ) -> Result<ToolAuthorizationDecision, RuntimeError> {
        self.0.lock().unwrap().push(request.clone());
        Ok(if request.tool_name == "denied" {
            ToolAuthorizationDecision::deny("approval denied by host")
        } else {
            ToolAuthorizationDecision::allow()
        })
    }
}

fn provider(streams: Vec<StreamScript>) -> ScriptedProvider {
    ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model_info(MODEL, BuiltinProvider::Anthropic)],
        streams,
    )
}

fn script_stream() -> StreamScript {
    tool_use_stream(MODEL, SCRIPT_CALL, "script", "{}")
}

fn call(name: &'static str) -> (&'static str, Option<Value>) {
    (name, Some(json!({})))
}

fn result(agent: &Agent, id: &str) -> ReviewedToolResult {
    agent
        .history()
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } if tool_use_id == id => Some(ReviewedToolResult {
                content: content.clone(),
                is_error: *is_error,
            }),
            _ => None,
        })
        .expect("provider-visible result")
}

fn journal(transcript: &AgentTranscript) -> Vec<(&str, &Value)> {
    transcript
        .items()
        .iter()
        .filter_map(|item| match &item.kind {
            TranscriptKind::ToolExchange {
                tool_use_id: Some(id),
                ..
            } if item.message.is_none() => Some((id.as_str(), item.detail(id).unwrap())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn child_dispatch_matches_direct_visibility_admission_and_review() {
    let provider = provider(vec![
        script_stream(),
        text_stream(MODEL, "after script"),
        multi_tool_use_stream(
            MODEL,
            &[
                ("direct", "consume", r#"{"id":"reviewed"}"#),
                ("direct-denied", "denied", "{}"),
                ("direct-foreign", "foreign", "{}"),
                ("direct-excluded", "excluded", "{}"),
                ("direct-invalid", "consume", r#"{"id":7}"#),
            ],
        ),
        text_stream(MODEL, "after direct"),
    ]);
    let lookup = Leaf::new(
        "lookup",
        ToolOutput::structured(json!({"id": "before-review"}))
            .with_details(json!({"diagnostic": "host-only"})),
    );
    let mut consume = Leaf::new("consume", ToolOutput::text(r#"{"still":"text"}"#));
    consume.spec = ToolSpec::builder("consume")
        .input_schema(json!({
            "type": "object",
            "required": ["id"],
            "properties": {"id": {"type": "string"}}
        }))
        .execution_category(ToolExecutionCategory::ReadOnlyParallel)
        .build();
    let consumed = consume.inputs.clone();
    let refused = Arc::new(Mutex::new(Vec::new()));
    let uncallable = |name| Leaf {
        inputs: refused.clone(),
        ..Leaf::new(name, ToolOutput::text("unexpected"))
    };
    let authorizer = Authorizer::default();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider.clone())
        .with_tool(lookup)
        .with_tool(consume)
        .with_tool(uncallable("excluded"))
        .with_tool(uncallable("denied"))
        .with_tool_authorizer(authorizer.clone())
        .with_pre_hook(Rewrite)
        .with_post_hook(Rewrite)
        .build()
        .unwrap();
    let _foreign = runtime
        .try_register_tool_for_audience(ToolAudience::new("other"), uncallable("foreign"))
        .unwrap();
    let config = AgentConfig {
        tool_profile: ToolProfile::hide(["excluded"]),
        ..Default::default()
    };
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model_info(MODEL, BuiltinProvider::Anthropic),
            config,
        )
        .unwrap();
    let script = Composer::new(
        "script",
        vec![
            call("lookup"),
            ("consume", None),
            call("denied"),
            call("foreign"),
            call("excluded"),
            ("consume", Some(json!({"id": 7}))),
        ],
    );
    let observed = script.observed.clone();
    agent.register_tool(script);
    let mut events = agent.subscribe_events();

    agent
        .send(vec![ContentBlock::text("compose")])
        .await
        .unwrap();
    agent
        .send(vec![ContentBlock::text("direct")])
        .await
        .unwrap();

    let requests = provider.recorded_requests().await;
    assert_eq!(
        requests[1].messages.last().unwrap().content,
        [ContentBlock::ToolResult {
            tool_use_id: SCRIPT_CALL.to_string(),
            content: "selected result".into(),
            is_error: false,
        }],
    );
    let visible_messages = serde_json::to_string(&requests[1].messages).unwrap();
    for private in ["before-review", "reviewed", "host-only", "still"] {
        assert!(!visible_messages.contains(private), "{private} leaked");
    }
    let observed = observed.lock().unwrap();
    assert_eq!(&*observed.offered, requests[0].tools.as_ref());
    assert_eq!(
        observed.results[0].content,
        ToolResultContent::Structured(json!({"id": "reviewed"})),
    );
    assert_eq!(
        observed.results[1].content,
        ToolResultContent::Text(r#"{"still":"text"}"#.to_string()),
    );
    assert_eq!(observed.catches, 4);
    for (child, direct_id) in observed.results[1..].iter().zip([
        "direct",
        "direct-denied",
        "direct-foreign",
        "direct-excluded",
        "direct-invalid",
    ]) {
        assert_eq!(*child, result(&agent, direct_id));
    }
    assert!(refused.lock().unwrap().is_empty());
    assert_eq!(
        *consumed.lock().unwrap(),
        [json!({"id": "admitted"}), json!({"id": "admitted"})],
    );
    let approvals = authorizer.0.lock().unwrap();
    let names = approvals
        .iter()
        .map(|request| request.tool_name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        ["script", "lookup", "consume", "denied", "consume", "denied"]
    );
    assert_eq!(approvals[2].preview.raw_input, json!({"id": "admitted"}));
    assert_eq!(
        approvals[2].preview.execution_category,
        ToolExecutionCategory::ReadOnlyParallel
    );

    let audit = journal(agent.transcript());
    assert_eq!(audit.len(), 12);
    for ((pair, reviewed), name) in audit.chunks_exact(2).zip(&observed.results).zip([
        "lookup", "consume", "denied", "foreign", "excluded", "consume",
    ]) {
        assert_eq!(pair[0].0, pair[1].0);
        assert_eq!(
            pair[0].1,
            &json!({
                "parent_tool_use_id": SCRIPT_CALL, "tool_name": name, "phase": "started",
            })
        );
        assert_eq!(
            pair[1].1,
            &json!({
                "parent_tool_use_id": SCRIPT_CALL, "tool_name": name, "phase": "finished",
                "is_error": reviewed.is_error,
            })
        );
    }
    let lookup_id = audit[0].0;
    let mut saw_start = false;
    let mut saw_finish = false;
    while let Ok(event) = events.try_recv() {
        match event {
            AgentEvent::ToolExecutionStarted { call } if call.id == lookup_id => saw_start = true,
            AgentEvent::ToolExecutionFinished {
                result: ContentBlock::ToolResult { tool_use_id, .. },
            } if tool_use_id == lookup_id => saw_finish = true,
            _ => {}
        }
    }
    assert!(saw_start && saw_finish);
}

#[tokio::test]
async fn a_stop_cannot_be_caught_and_followed_by_another_child_call() {
    let stop = CancellationToken::default();
    let provider = provider(vec![script_stream()]);
    let second = Leaf::new("second", ToolOutput::text("unexpected"));
    let invocations = second.inputs.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider.clone())
        .with_tool(StopTrippingTool::new("stop", stop.clone()))
        .with_tool(second)
        .build()
        .unwrap();
    let mut agent = runtime
        .spawn("agent", model_info(MODEL, BuiltinProvider::Anthropic))
        .unwrap();
    let script = Composer::new("script", vec![call("stop"), call("second")]);
    let observed = script.observed.clone();
    agent.register_tool(script);

    let options = RunOptions {
        stop: Some(stop.clone()),
        ..Default::default()
    };
    let outcome = agent
        .run(vec![ContentBlock::text("compose")], options.clone())
        .await;
    assert!(matches!(outcome, Err(RuntimeError::EmptyAssistantResponse)));
    assert!(stop.is_cancelled());
    assert_eq!(options.ended_early(), Some(EarlyEnd::StopRequested));
    assert!(invocations.lock().unwrap().is_empty());
    {
        let observed = observed.lock().unwrap();
        assert_eq!(observed.catches, 0);
        assert!(observed.results.is_empty());
    }
    assert!(result(&agent, SCRIPT_CALL).is_error);
    assert_eq!(provider.recorded_requests().await.len(), 1);
}

#[tokio::test]
async fn a_nested_terminal_result_ends_evaluation_and_skips_later_calls() {
    let provider = provider(vec![multi_tool_use_stream(
        MODEL,
        &[(SCRIPT_CALL, "script", "{}"), ("sibling", "second", "{}")],
    )]);
    let mut terminal = Leaf::new(
        "finish",
        ToolOutput::structured(json!({"final": 8})).terminating(),
    );
    terminal.spec = ToolSpec::builder("finish").terminal().build();
    let second = Leaf::new("second", ToolOutput::text("unexpected"));
    let invocations = second.inputs.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider.clone())
        .with_tool(terminal)
        .with_tool(second)
        .build()
        .unwrap();
    let mut agent = runtime
        .spawn("agent", model_info(MODEL, BuiltinProvider::Anthropic))
        .unwrap();
    let script = Composer::new("script", vec![call("finish"), call("second")]);
    let observed = script.observed.clone();
    agent.register_tool(script);

    let outcome = agent.send(vec![ContentBlock::text("compose")]).await;
    assert!(matches!(outcome, Err(RuntimeError::EmptyAssistantResponse)));
    assert!(invocations.lock().unwrap().is_empty());
    assert!(observed.lock().unwrap().results.is_empty());
    assert_eq!(
        result(&agent, SCRIPT_CALL),
        ReviewedToolResult {
            content: ToolResultContent::Structured(json!({"final": 8})),
            is_error: false,
        },
    );
    assert!(result(&agent, "sibling").is_error);
    assert_eq!(provider.recorded_requests().await.len(), 1);
}

#[tokio::test]
async fn children_share_the_turn_budget_and_a_failure_cannot_replay_the_parent() {
    let provider = provider(vec![script_stream(), text_stream(MODEL, "checked; done")]);
    let leaf = Leaf::new("effect", ToolOutput::text("effect complete"));
    let invocations = leaf.inputs.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider.clone())
        .with_tool(leaf)
        .build()
        .unwrap();
    let mut agent = runtime
        .spawn("agent", model_info(MODEL, BuiltinProvider::Anthropic))
        .unwrap();
    let script = Composer::new("script", vec![call("effect"), call("effect")]);
    let observed = script.observed.clone();
    agent.register_tool(script);

    let outcome = agent
        .run(
            vec![ContentBlock::text("compose")],
            RunOptions {
                tool_budget: Some(2),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(outcome, Err(RuntimeError::ToolBudgetExceeded(2))));
    assert_eq!(observed.lock().unwrap().results.len(), 1);
    assert_eq!(invocations.lock().unwrap().len(), 1);
    assert!(agent.has_interrupted_turn());
    assert!(matches!(
        agent.resume().await,
        Err(RuntimeError::NoResumableTurn)
    ));
    agent
        .continue_interrupted(RunOptions::default())
        .await
        .unwrap();
    assert_eq!(invocations.lock().unwrap().len(), 1);
    assert_eq!(provider.recorded_requests().await.len(), 2);
}

struct PendingApproval(Arc<Notify>);

#[async_trait]
impl ToolAuthorizer for PendingApproval {
    async fn authorize(
        &self,
        request: &ToolAuthorizationRequest,
    ) -> Result<ToolAuthorizationDecision, RuntimeError> {
        if request.tool_name == "waiting" {
            self.0.notify_one();
            return pending().await;
        }
        Ok(ToolAuthorizationDecision::allow())
    }
}

#[tokio::test]
async fn cancelling_a_pending_child_approval_drops_the_evaluation() {
    let approval = Arc::new(Notify::new());
    let leaf = Leaf::new("waiting", ToolOutput::text("unexpected"));
    let invocations = leaf.inputs.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider(vec![script_stream()]))
        .with_tool(leaf)
        .with_tool_authorizer(PendingApproval(approval.clone()))
        .build()
        .unwrap();
    let mut agent = runtime
        .spawn("agent", model_info(MODEL, BuiltinProvider::Anthropic))
        .unwrap();
    let script = Composer::new("script", vec![call("waiting"), call("waiting")]);
    let observed = script.observed.clone();
    agent.register_tool(script);
    let cancellation = CancellationToken::default();
    let options = RunOptions {
        cancellation: Some(cancellation.clone()),
        ..Default::default()
    };
    let turn = tokio::spawn(async move {
        let outcome = agent
            .run(vec![ContentBlock::text("compose")], options)
            .await;
        (agent, outcome)
    });

    timeout(Duration::from_secs(5), approval.notified())
        .await
        .unwrap();
    cancellation.cancel();
    let (agent, outcome) = timeout(Duration::from_secs(5), turn)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(outcome, Err(RuntimeError::Cancelled)));
    assert!(observed.lock().unwrap().results.is_empty());
    assert!(invocations.lock().unwrap().is_empty());
    let audit = journal(agent.transcript());
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].1["phase"], "started");
    assert!(agent.has_interrupted_turn());
}

struct InFlight {
    ran: Arc<AtomicUsize>,
    started: Arc<Notify>,
}

impl ToolDefinition for InFlight {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("in_flight").build()
    }
}

#[async_trait]
impl ToolExecutor for InFlight {
    async fn execute_mut_output(
        &self,
        _ctx: ToolContext<'_>,
        _input: Value,
    ) -> Result<ToolOutput, String> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        pending().await
    }
}

#[tokio::test]
async fn a_durable_interruption_leaves_the_child_linked_without_replaying_it() {
    let store = temp_store("nested-dispatch");
    let ran = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let model = model_info(MODEL, BuiltinProvider::Anthropic);
    let runtime = Runtime::empty_builder()
        .with_store(store.clone())
        .with_provider_instance(provider(vec![script_stream()]))
        .with_tool(InFlight {
            ran: ran.clone(),
            started: started.clone(),
        })
        .build()
        .unwrap();
    let mut agent = runtime.spawn("agent", model.clone()).unwrap();
    let agent_id = agent.id().to_string();
    agent.register_tool(Composer::new("script", vec![call("in_flight")]));
    let turn = tokio::spawn(async move { agent.send(vec![ContentBlock::text("compose")]).await });
    timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    turn.abort();
    assert!(turn.await.unwrap_err().is_cancelled());
    clear_leases(&store);

    let resumed_provider = provider(vec![text_stream(MODEL, "checked; done")]);
    let restarted = Runtime::empty_builder()
        .with_store(store)
        .with_provider_instance(resumed_provider.clone())
        .with_tool(InFlight {
            ran: ran.clone(),
            started,
        })
        .build()
        .unwrap();
    let mut resumed = restarted.resume_agent(&agent_id).unwrap();
    resumed.register_tool(Composer::new("script", vec![call("in_flight")]));
    assert!(resumed.has_interrupted_turn());
    assert!(matches!(
        resumed.resume().await,
        Err(RuntimeError::NoResumableTurn)
    ));
    let audit = journal(resumed.transcript());
    assert_eq!(audit.len(), 1);
    assert_eq!(
        audit[0].1,
        &json!({
            "parent_tool_use_id": SCRIPT_CALL, "tool_name": "in_flight", "phase": "started",
        })
    );
    assert!(
        result(&resumed, SCRIPT_CALL)
            .content
            .to_display_string()
            .contains("effect is unknown")
    );
    resumed
        .continue_interrupted(RunOptions::default())
        .await
        .unwrap();
    assert_eq!(ran.load(Ordering::SeqCst), 1);
    let requests = resumed_provider.recorded_requests().await;
    assert_eq!(requests[0].messages.len(), 3);
    assert!(
        !serde_json::to_string(&requests[0].messages)
            .unwrap()
            .contains("in_flight")
    );
}

#[tokio::test]
async fn child_dispatch_reresolves_the_name_after_discovery_and_refuses_ancestors() {
    let provider = provider(vec![script_stream(), text_stream(MODEL, "done")]);
    let initial = Leaf::new("changeable", ToolOutput::text("old"));
    let old_runs = initial.inputs.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(initial)
        .build()
        .unwrap();
    let mut agent = runtime
        .spawn("agent", model_info(MODEL, BuiltinProvider::Anthropic))
        .unwrap();
    let mut script = Composer::new(
        "script",
        vec![call("changeable"), call("script"), call("helper")],
    );
    let helper = Composer::new("helper", vec![call("script")]);
    let observed = script.observed.clone();
    let helper_observed = helper.observed.clone();
    let ready = Arc::new(Notify::new());
    let go = Arc::new(Notify::new());
    script.after_discovery = Some((ready.clone(), go.clone()));
    agent.register_tool(script);
    agent.register_tool(helper);
    let turn = tokio::spawn(async move { agent.send(vec![ContentBlock::text("compose")]).await });
    timeout(Duration::from_secs(5), ready.notified())
        .await
        .unwrap();
    let replacement = Leaf::new("changeable", ToolOutput::text("new"));
    let new_runs = replacement.inputs.clone();
    runtime.register_tool(replacement);
    go.notify_one();
    timeout(Duration::from_secs(5), turn)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    assert!(old_runs.lock().unwrap().is_empty());
    assert_eq!(new_runs.lock().unwrap().len(), 1);
    let observed = observed.lock().unwrap();
    assert_eq!(observed.results[0].content, ToolResultContent::text("new"));
    assert!(observed.results[1].is_error);
    assert_eq!(observed.catches, 1);
    assert!(helper_observed.lock().unwrap().results[0].is_error);
}

#[tokio::test]
async fn a_deadline_interrupts_a_pending_child_and_drops_the_evaluator() {
    let started = Arc::new(Notify::new());
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::empty_builder()
        // Expiration here measures a pending handler, independently of the
        // file-store writes exercised by the durable interruption test.
        .with_store(crate::runtime::VolatileRuntimeStore::new())
        .with_provider_instance(provider(vec![script_stream()]))
        .with_tool(InFlight {
            ran: ran.clone(),
            started: started.clone(),
        })
        .build()
        .unwrap();
    let mut agent = runtime
        .spawn("agent", model_info(MODEL, BuiltinProvider::Anthropic))
        .unwrap();
    let script = Composer::new("script", vec![call("in_flight"), call("in_flight")]);
    let observed = script.observed.clone();
    agent.register_tool(script);

    let options = RunOptions {
        deadline: Some(SystemTime::now() + Duration::from_secs(2)),
        ..Default::default()
    };
    let outcome = timeout(
        Duration::from_secs(5),
        agent.run(vec![ContentBlock::text("compose")], options),
    )
    .await
    .expect("the dispatcher must wake the pending evaluator");
    assert!(matches!(outcome, Err(RuntimeError::DeadlineExceeded)));
    assert_eq!(ran.load(Ordering::SeqCst), 1);
    assert!(observed.lock().unwrap().results.is_empty());
    assert!(agent.has_interrupted_turn());
}

#[tokio::test]
async fn dropping_a_run_future_keeps_the_live_agent_from_replaying_children() {
    let started = Arc::new(Notify::new());
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider(vec![script_stream(), text_stream(MODEL, "done")]))
        .with_tool(InFlight {
            ran: ran.clone(),
            started: started.clone(),
        })
        .build()
        .unwrap();
    let mut agent = runtime
        .spawn("agent", model_info(MODEL, BuiltinProvider::Anthropic))
        .unwrap();
    agent.register_tool(Composer::new("script", vec![call("in_flight")]));

    {
        let turn = agent.send(vec![ContentBlock::text("compose")]);
        tokio::select! {
            result = turn => panic!("child must still be pending: {result:?}"),
            begun = timeout(Duration::from_secs(5), started.notified()) => begun.unwrap(),
        }
    }
    assert!(agent.has_interrupted_turn());
    assert!(matches!(
        agent.resume().await,
        Err(RuntimeError::NoResumableTurn)
    ));
    agent
        .continue_interrupted(RunOptions::default())
        .await
        .unwrap();
    assert_eq!(ran.load(Ordering::SeqCst), 1);
    assert!(result(&agent, SCRIPT_CALL).is_error);
}
