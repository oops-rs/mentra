//! The order in which a scheduled call meets its pre-execution hooks, the
//! schema check, and the `ToolAuthorizer` — and that both execution lanes
//! agree on it.
//!
//! Every test runs twice, once through the serial lane and once through the
//! parallel lane, because the two lanes are separate code paths and the
//! ordering is a contract hosts build permission ladders on.

use std::sync::{
    Arc, Mutex, OnceLock, Weak,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::{
    BuiltinProvider, ContentBlock,
    runtime::{
        Runtime, RuntimeError, RuntimeHook, RuntimeHookEvent,
        control::{
            HookDecision, PostExecutionContext, PostExecutionHook, PreExecutionContext,
            PreExecutionHook, ResultDecision,
        },
    },
    session::{
        PermissionRuleScope, RememberedRule, RuleKey,
        permission::{PendingPermissionStore, RuleStore, SessionToolAuthorizer},
    },
    tool::{
        ParallelToolContext, ToolAuthorizationDecision, ToolAuthorizationRequest, ToolAuthorizer,
        ToolDefinition, ToolDurability, ToolExecutionCategory, ToolExecutor, ToolResult,
        ToolSideEffectLevel, ToolSpec,
    },
};

use super::support::{ScriptedProvider, model_info, text_stream, tool_use_stream};

const TOOL: &str = "gate_tool";
const ORIGINAL: &str = r#"{"command":"rm -rf /"}"#;
const REWRITTEN: &str = r#"{"command":"ls"}"#;

/// A tool with a real schema whose lane the test picks, recording what it ran
/// with.
struct GateTool {
    parallel: bool,
    ran: Arc<Mutex<Vec<Value>>>,
}

#[async_trait]
impl ToolDefinition for GateTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder(TOOL)
            .description("a gated tool")
            .input_schema(json!({
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"]
            }))
            .side_effect_level(ToolSideEffectLevel::None)
            .durability(ToolDurability::ReplaySafe)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for GateTool {
    fn execution_category(&self, _input: &Value) -> ToolExecutionCategory {
        if self.parallel {
            ToolExecutionCategory::ReadOnlyParallel
        } else {
            ToolExecutionCategory::ExclusiveLocalMutation
        }
    }

    async fn execute(&self, _ctx: ParallelToolContext, input: Value) -> ToolResult {
        self.ran.lock().expect("ran poisoned").push(input);
        Ok("ran".to_string())
    }
}

struct GenerationTool {
    label: &'static str,
    execution_category: ToolExecutionCategory,
    ran: Arc<AtomicUsize>,
}

impl ToolDefinition for GenerationTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder(TOOL)
            .description(self.label)
            .execution_category(self.execution_category)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for GenerationTool {
    fn execution_category(&self, _input: &Value) -> ToolExecutionCategory {
        self.execution_category
    }

    async fn execute(&self, _ctx: ParallelToolContext, _input: Value) -> ToolResult {
        self.ran.fetch_add(1, Ordering::SeqCst);
        Ok(self.label.to_string())
    }
}

struct ReplaceGenerationDuringAdmission {
    runtime: Arc<OnceLock<Weak<Runtime>>>,
    replacement: Mutex<Option<GenerationTool>>,
}

#[async_trait]
impl PreExecutionHook for ReplaceGenerationDuringAdmission {
    async fn pre_tool_execution(
        &self,
        _context: &PreExecutionContext,
    ) -> Result<HookDecision, RuntimeError> {
        if let Some(replacement) = self
            .replacement
            .lock()
            .expect("replacement poisoned")
            .take()
        {
            self.runtime
                .get()
                .and_then(Weak::upgrade)
                .expect("runtime installed before execution")
                .register_tool(replacement);
        }
        Ok(HookDecision::Allow)
    }
}

struct Rewrite(&'static str);

#[async_trait]
impl PreExecutionHook for Rewrite {
    async fn pre_tool_execution(
        &self,
        _context: &PreExecutionContext,
    ) -> Result<HookDecision, RuntimeError> {
        Ok(HookDecision::Modify {
            input_json: self.0.to_string(),
            reason: None,
        })
    }
}

/// Records the input it was shown, then denies — the side effect a host must
/// now expect for a call the authorizer would have refused.
struct Refuse(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl PreExecutionHook for Refuse {
    async fn pre_tool_execution(
        &self,
        context: &PreExecutionContext,
    ) -> Result<HookDecision, RuntimeError> {
        self.0
            .lock()
            .expect("hook log poisoned")
            .push(context.input_json.clone());
        Ok(HookDecision::Deny("hook said no".to_string()))
    }
}

struct Observe(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl PreExecutionHook for Observe {
    async fn pre_tool_execution(
        &self,
        context: &PreExecutionContext,
    ) -> Result<HookDecision, RuntimeError> {
        self.0
            .lock()
            .expect("hook log poisoned")
            .push(context.input_json.clone());
        Ok(HookDecision::Allow)
    }
}

struct RecordsFinalInput(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl PostExecutionHook for RecordsFinalInput {
    async fn post_tool_execution(
        &self,
        context: &PostExecutionContext,
    ) -> Result<ResultDecision, RuntimeError> {
        self.0
            .lock()
            .expect("post log poisoned")
            .push(context.input_json.clone());
        Ok(ResultDecision::Keep)
    }
}

struct RecordingAuthorizer {
    allow: bool,
    requests: Arc<Mutex<Vec<ToolAuthorizationRequest>>>,
}

#[async_trait]
impl ToolAuthorizer for RecordingAuthorizer {
    async fn authorize(
        &self,
        request: &ToolAuthorizationRequest,
    ) -> Result<ToolAuthorizationDecision, RuntimeError> {
        self.requests
            .lock()
            .expect("requests poisoned")
            .push(request.clone());
        Ok(if self.allow {
            ToolAuthorizationDecision::allow()
        } else {
            ToolAuthorizationDecision::deny("authorizer said no")
        })
    }
}

struct RecordingHook(Arc<Mutex<Vec<RuntimeHookEvent>>>);

impl RuntimeHook for RecordingHook {
    fn on_event(
        &self,
        _store: &dyn crate::runtime::AuditStore,
        event: &RuntimeHookEvent,
    ) -> Result<(), RuntimeError> {
        self.0.lock().expect("events poisoned").push(event.clone());
        Ok(())
    }
}

/// What one run left behind, for either lane.
struct Outcome {
    tool_result: ContentBlock,
    ran: Vec<Value>,
    authorized: Vec<ToolAuthorizationRequest>,
    post_inputs: Vec<String>,
    hook_events: Vec<RuntimeHookEvent>,
}

struct Case {
    parallel: bool,
    pre_hook: Option<Arc<dyn PreExecutionHook>>,
    authorizer: Option<Arc<dyn ToolAuthorizer>>,
    requests: Arc<Mutex<Vec<ToolAuthorizationRequest>>>,
}

impl Case {
    fn new(parallel: bool) -> Self {
        Self {
            parallel,
            pre_hook: None,
            authorizer: None,
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn pre_hook(self, hook: impl PreExecutionHook + 'static) -> Self {
        Self {
            pre_hook: Some(Arc::new(hook)),
            ..self
        }
    }

    fn authorizer(self, allow: bool) -> Self {
        let authorizer = RecordingAuthorizer {
            allow,
            requests: Arc::clone(&self.requests),
        };
        Self {
            authorizer: Some(Arc::new(authorizer)),
            ..self
        }
    }

    fn session_authorizer(self, allow: bool, rules: RuleStore) -> Self {
        let inner = RecordingAuthorizer {
            allow,
            requests: Arc::clone(&self.requests),
        };
        let (event_tx, _) = broadcast::channel(8);
        let authorizer = SessionToolAuthorizer::new(
            Some(Arc::new(inner)),
            event_tx,
            PendingPermissionStore::new(),
            rules,
        );
        Self {
            authorizer: Some(Arc::new(authorizer)),
            ..self
        }
    }

    async fn run(self) -> Outcome {
        let ran = Arc::new(Mutex::new(Vec::new()));
        let post_inputs = Arc::new(Mutex::new(Vec::new()));
        let hook_events = Arc::new(Mutex::new(Vec::new()));
        let model = model_info("model", BuiltinProvider::Anthropic);
        let provider = ScriptedProvider::new(
            BuiltinProvider::Anthropic,
            vec![model.clone()],
            vec![
                tool_use_stream(&model.id, "call-1", TOOL, ORIGINAL),
                text_stream(&model.id, "done"),
            ],
        );

        let mut builder = Runtime::empty_builder()
            .with_provider_instance(provider)
            .with_tool(GateTool {
                parallel: self.parallel,
                ran: Arc::clone(&ran),
            })
            .with_post_hook(RecordsFinalInput(Arc::clone(&post_inputs)))
            .with_hook(RecordingHook(Arc::clone(&hook_events)));
        if let Some(hook) = self.pre_hook {
            builder = builder.with_pre_hook(hook);
        }
        if let Some(authorizer) = self.authorizer {
            builder = builder.with_tool_authorizer(authorizer);
        }
        let runtime = builder.build().expect("build runtime");

        let mut agent = runtime.spawn("agent", model).expect("spawn agent");
        agent
            .send(vec![ContentBlock::text("go")])
            .await
            .expect("run completes");

        let tool_result = agent
            .history()
            .iter()
            .flat_map(|message| message.content.iter())
            .find(|block| matches!(block, ContentBlock::ToolResult { .. }))
            .cloned()
            .expect("a tool result reaches the transcript");

        Outcome {
            tool_result,
            ran: ran.lock().expect("ran poisoned").clone(),
            authorized: self.requests.lock().expect("requests poisoned").clone(),
            post_inputs: post_inputs.lock().expect("post log poisoned").clone(),
            hook_events: hook_events.lock().expect("events poisoned").clone(),
        }
    }
}

fn result_text(block: &ContentBlock) -> (String, bool) {
    match block {
        ContentBlock::ToolResult {
            content, is_error, ..
        } => (content.to_display_string(), *is_error),
        other => panic!("not a tool result: {other:?}"),
    }
}

async fn run_files_rewrite(
    original: &'static str,
    rewritten: &'static str,
) -> (ContentBlock, Vec<ToolAuthorizationRequest>) {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "call-1", "files", original),
            text_stream(&model.id, "done"),
        ],
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .with_pre_hook(Rewrite(rewritten))
        .with_tool_authorizer(RecordingAuthorizer {
            allow: false,
            requests: Arc::clone(&requests),
        })
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");
    agent
        .send(vec![ContentBlock::text("go")])
        .await
        .expect("run completes");
    let result = agent
        .history()
        .iter()
        .flat_map(|message| message.content.iter())
        .find(|block| matches!(block, ContentBlock::ToolResult { .. }))
        .cloned()
        .expect("tool result");
    let requests = requests.lock().expect("requests poisoned").clone();
    (result, requests)
}

fn rewritten() -> Value {
    serde_json::from_str(REWRITTEN).expect("fixture parses")
}

fn original() -> Value {
    serde_json::from_str(ORIGINAL).expect("fixture parses")
}

fn allow_rule_for(pattern: &str) -> RuleStore {
    let store = RuleStore::new();
    store.add_rule(RememberedRule {
        key: RuleKey {
            tool_name: TOOL.to_string(),
            pattern: Some(pattern.to_string()),
        },
        allow: true,
        scope: PermissionRuleScope::Session,
        reason: None,
    });
    store
}

fn blocked_by_hook(events: &[RuntimeHookEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            RuntimeHookEvent::ToolExecutionBlocked { reason, .. } => Some(reason.as_str()),
            _ => None,
        })
        .collect()
}

fn authorization_started(events: &[RuntimeHookEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, RuntimeHookEvent::ToolAuthorizationStarted { .. }))
}

fn execution_started(events: &[RuntimeHookEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, RuntimeHookEvent::ToolExecutionStarted { .. }))
}

async fn for_both_lanes<F, Fut>(check: F)
where
    F: Fn(bool) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    check(false).await;
    check(true).await;
}

#[tokio::test]
async fn the_authorizer_is_asked_about_the_input_the_tool_runs_with() {
    for_both_lanes(|parallel| async move {
        let outcome = Case::new(parallel)
            .pre_hook(Rewrite(REWRITTEN))
            .authorizer(true)
            .run()
            .await;

        assert_eq!(outcome.ran, vec![rewritten()], "parallel={parallel}");
        assert_eq!(outcome.authorized.len(), 1, "parallel={parallel}");
        assert_eq!(
            outcome.authorized[0].preview.structured_input,
            rewritten(),
            "parallel={parallel}: the authorizer judged a call that never ran"
        );
        assert_eq!(
            outcome.post_inputs,
            vec![REWRITTEN.to_string()],
            "parallel={parallel}: the recorded input is the final input"
        );
        assert_eq!(
            result_text(&outcome.tool_result),
            ("ran".to_string(), false)
        );
    })
    .await;
}

#[tokio::test]
async fn a_remembered_rule_written_against_the_rewritten_input_answers_the_call() {
    for_both_lanes(|parallel| async move {
        let outcome = Case::new(parallel)
            .pre_hook(Rewrite(REWRITTEN))
            .session_authorizer(false, allow_rule_for(r#"{"command":"ls"}"#))
            .run()
            .await;

        assert_eq!(outcome.ran, vec![rewritten()], "parallel={parallel}");
        assert!(
            outcome.authorized.is_empty(),
            "parallel={parallel}: the rule answered, so the approver was not asked"
        );
    })
    .await;
}

#[tokio::test]
async fn a_remembered_rule_written_against_the_original_input_no_longer_matches() {
    for_both_lanes(|parallel| async move {
        let outcome = Case::new(parallel)
            .pre_hook(Rewrite(REWRITTEN))
            .session_authorizer(false, allow_rule_for(r#"{"command":"rm -rf /"}"#))
            .run()
            .await;

        assert!(outcome.ran.is_empty(), "parallel={parallel}");
        assert_eq!(
            outcome.authorized.len(),
            1,
            "parallel={parallel}: a rule for the discarded input does not answer"
        );
        assert_eq!(outcome.authorized[0].preview.structured_input, rewritten());
    })
    .await;
}

#[tokio::test]
async fn a_hook_rewriting_into_schema_invalid_input_is_refused_before_anyone_is_asked() {
    for_both_lanes(|parallel| async move {
        let outcome = Case::new(parallel)
            .pre_hook(Rewrite(r#"{"command":42}"#))
            .authorizer(true)
            .run()
            .await;

        assert!(outcome.ran.is_empty(), "parallel={parallel}");
        assert!(outcome.authorized.is_empty(), "parallel={parallel}");
        assert!(!authorization_started(&outcome.hook_events));
        assert!(!execution_started(&outcome.hook_events));
        let (text, is_error) = result_text(&outcome.tool_result);
        assert!(is_error);
        assert!(
            text.contains("pre-execution hook") && text.contains("schema"),
            "parallel={parallel}: the message names the hook as the source: {text}"
        );
        let blocked = blocked_by_hook(&outcome.hook_events);
        assert_eq!(blocked.len(), 1, "parallel={parallel}");
        assert!(blocked[0].contains("pre-execution hook"));
    })
    .await;
}

#[tokio::test]
async fn a_files_hook_cannot_turn_a_parallel_read_into_an_exclusive_write() {
    let (result, requests) = run_files_rewrite(
        r#"{"operations":[{"op":"read","path":"missing.txt"}]}"#,
        r#"{"operations":[{"op":"create","path":"created.txt","content":"write"}]}"#,
    )
    .await;

    assert!(
        requests.is_empty(),
        "unsafe rewrite stops before authorization"
    );
    let (text, is_error) = result_text(&result);
    assert!(is_error);
    assert!(
        text.contains("parallel lane") && text.contains("pre-execution hook"),
        "{text}"
    );
}

#[tokio::test]
async fn an_exclusive_files_call_stays_serial_when_a_hook_rewrites_it_to_a_read() {
    let (result, requests) = run_files_rewrite(
        r#"{"operations":[{"op":"create","path":"created.txt","content":"write"}]}"#,
        r#"{"operations":[{"op":"read","path":"missing.txt"}]}"#,
    )
    .await;

    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].preview.execution_category,
        ToolExecutionCategory::ReadOnlyParallel,
        "the approver sees the rewritten call's category while execution stays on its serial lane"
    );
    assert_eq!(
        result_text(&result),
        (
            "Tool execution denied: authorizer said no".to_string(),
            true
        )
    );
}

#[tokio::test]
async fn a_hook_denial_short_circuits_before_the_authorizer() {
    for_both_lanes(|parallel| async move {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let outcome = Case::new(parallel)
            .pre_hook(Refuse(Arc::clone(&seen)))
            .authorizer(true)
            .run()
            .await;

        assert!(outcome.ran.is_empty(), "parallel={parallel}");
        assert!(outcome.authorized.is_empty(), "parallel={parallel}");
        assert!(!authorization_started(&outcome.hook_events));
        assert_eq!(blocked_by_hook(&outcome.hook_events), vec!["hook said no"]);
        let (text, is_error) = result_text(&outcome.tool_result);
        assert!(is_error);
        assert_eq!(text, "Blocked by pre-execution hook: hook said no");
    })
    .await;
}

#[tokio::test]
async fn a_hook_runs_even_for_a_call_the_authorizer_refuses() {
    for_both_lanes(|parallel| async move {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let outcome = Case::new(parallel)
            .pre_hook(Observe(Arc::clone(&seen)))
            .authorizer(false)
            .run()
            .await;

        assert_eq!(
            *seen.lock().expect("hook log poisoned"),
            vec![ORIGINAL.to_string()],
            "parallel={parallel}: the hook sees the call before the authorizer refuses it"
        );
        assert_eq!(outcome.authorized.len(), 1);
        assert!(outcome.ran.is_empty());
        assert!(blocked_by_hook(&outcome.hook_events).is_empty());
        let (text, is_error) = result_text(&outcome.tool_result);
        assert!(is_error);
        assert_eq!(text, "Tool execution denied: authorizer said no");
    })
    .await;
}

#[tokio::test]
async fn an_allowing_hook_changes_nothing() {
    for_both_lanes(|parallel| async move {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let outcome = Case::new(parallel)
            .pre_hook(Observe(Arc::clone(&seen)))
            .authorizer(true)
            .run()
            .await;

        assert_eq!(outcome.ran, vec![original()], "parallel={parallel}");
        assert_eq!(outcome.authorized.len(), 1);
        assert_eq!(outcome.authorized[0].preview.structured_input, original());
        assert_eq!(outcome.post_inputs, vec![ORIGINAL.to_string()]);
        assert!(execution_started(&outcome.hook_events));
        assert_eq!(
            result_text(&outcome.tool_result),
            ("ran".to_string(), false)
        );
    })
    .await;
}

#[tokio::test]
async fn a_call_executes_the_generation_snapshot_the_scheduler_classified() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "call-1", TOOL, r#"{}"#),
            text_stream(&model.id, "done"),
        ],
    );
    let original_ran = Arc::new(AtomicUsize::new(0));
    let replacement_ran = Arc::new(AtomicUsize::new(0));
    let runtime_slot = Arc::new(OnceLock::new());
    let runtime = Arc::new(
        Runtime::empty_builder()
            .with_provider_instance(provider)
            .with_tool(GenerationTool {
                label: "original",
                execution_category: ToolExecutionCategory::ReadOnlyParallel,
                ran: Arc::clone(&original_ran),
            })
            .with_pre_hook(ReplaceGenerationDuringAdmission {
                runtime: Arc::clone(&runtime_slot),
                replacement: Mutex::new(Some(GenerationTool {
                    label: "replacement",
                    execution_category: ToolExecutionCategory::ExclusiveLocalMutation,
                    ran: Arc::clone(&replacement_ran),
                })),
            })
            .build()
            .expect("build runtime"),
    );
    runtime_slot
        .set(Arc::downgrade(&runtime))
        .expect("runtime installed once");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    agent
        .send(vec![ContentBlock::text("go")])
        .await
        .expect("run completes");

    assert_eq!(original_ran.load(Ordering::SeqCst), 1);
    assert_eq!(replacement_ran.load(Ordering::SeqCst), 0);
    assert_eq!(
        runtime
            .tool_descriptor(TOOL)
            .expect("replacement remains registered")
            .provider
            .description
            .as_deref(),
        Some("replacement")
    );
    let result = agent
        .history()
        .iter()
        .flat_map(|message| message.content.iter())
        .find(|block| matches!(block, ContentBlock::ToolResult { .. }))
        .expect("tool result");
    assert_eq!(result_text(result), ("original".to_string(), false));
}

#[tokio::test]
async fn the_models_own_schema_error_is_still_answered_to_the_model() {
    for_both_lanes(|parallel| async move {
        let model = model_info("model", BuiltinProvider::Anthropic);
        let provider = ScriptedProvider::new(
            BuiltinProvider::Anthropic,
            vec![model.clone()],
            vec![
                tool_use_stream(&model.id, "call-1", TOOL, r#"{"command":42}"#),
                text_stream(&model.id, "done"),
            ],
        );
        let ran = Arc::new(Mutex::new(Vec::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::empty_builder()
            .with_provider_instance(provider)
            .with_tool(GateTool {
                parallel,
                ran: Arc::clone(&ran),
            })
            .with_pre_hook(Observe(Arc::new(Mutex::new(Vec::new()))))
            .with_tool_authorizer(RecordingAuthorizer {
                allow: true,
                requests: Arc::clone(&requests),
            })
            .build()
            .expect("build runtime");
        let mut agent = runtime.spawn("agent", model).expect("spawn agent");
        agent
            .send(vec![ContentBlock::text("go")])
            .await
            .expect("run completes");

        let result = agent
            .history()
            .iter()
            .flat_map(|message| message.content.iter())
            .find(|block| matches!(block, ContentBlock::ToolResult { .. }))
            .cloned()
            .expect("a tool result reaches the transcript");
        let (text, is_error) = result_text(&result);
        assert!(is_error, "parallel={parallel}");
        assert!(
            text.starts_with("Invalid input for 'gate_tool':"),
            "parallel={parallel}: the model, not a hook, is told what to fix: {text}"
        );
        assert!(ran.lock().expect("ran poisoned").is_empty());
        assert!(requests.lock().expect("requests poisoned").is_empty());
    })
    .await;
}
