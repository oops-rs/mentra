use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use tokio::sync::Notify;

use crate::{
    AgentConfig, BuiltinProvider, ContentBlock, Message, Runtime, ToolAudience,
    error::RuntimeError,
    provider::{ContentBlockDelta, ContentBlockStart, ProviderEvent, Role},
    runtime::{
        AfterDecision, BeforeDecision, ExecutionHookParticipant, HookDecision,
        PostExecutionContext, PostExecutionHook, PreExecutionContext, PreExecutionHook,
        ResultDecision, RuntimePolicy, SessionOptions, VolatileRuntimeStore,
    },
    tool::ToolResultContent,
};

use super::support::{
    ProbeTool, ScriptedProvider, StaticTool, model_info, ok_stream, text_stream, tool_use_stream,
};

fn result_for(history: &[Message], call_id: &str) -> (String, bool) {
    history
        .iter()
        .flat_map(|message| message.content.iter())
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } if tool_use_id == call_id => Some((content.to_display_string(), *is_error)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing result for {call_id}"))
}

struct LegacyPre(Arc<Mutex<Vec<&'static str>>>);

#[async_trait]
impl PreExecutionHook for LegacyPre {
    async fn pre_tool_execution(
        &self,
        _context: &PreExecutionContext,
    ) -> Result<HookDecision, RuntimeError> {
        self.0.lock().expect("log").push("legacy-pre");
        Ok(HookDecision::Allow)
    }
}

struct LegacyPost(Arc<Mutex<Vec<&'static str>>>);

#[async_trait]
impl PostExecutionHook for LegacyPost {
    async fn post_tool_execution(
        &self,
        context: &PostExecutionContext,
    ) -> Result<ResultDecision, RuntimeError> {
        self.0.lock().expect("log").push("legacy-post");
        Ok(ResultDecision::Replace {
            content: ToolResultContent::text(format!(
                "{}-legacy",
                context.content.to_display_string()
            )),
            is_error: context.is_error,
        })
    }
}

struct OrderedMixed {
    name: &'static str,
    log: Arc<Mutex<Vec<&'static str>>>,
}

#[async_trait]
impl ExecutionHookParticipant for OrderedMixed {
    fn name(&self) -> &str {
        self.name
    }

    async fn before(&self, _context: &PreExecutionContext) -> Result<BeforeDecision, RuntimeError> {
        self.log.lock().expect("log").push(match self.name {
            "host" => "host-before",
            _ => "workspace-before",
        });
        Ok(BeforeDecision::Continue)
    }

    async fn after(&self, context: &PostExecutionContext) -> Result<AfterDecision, RuntimeError> {
        self.log.lock().expect("log").push(match self.name {
            "host" => "host-after",
            _ => "workspace-after",
        });
        Ok(AfterDecision::Replace {
            content: ToolResultContent::text(format!(
                "{}-{}",
                context.content.to_display_string(),
                self.name
            )),
            is_error: None,
            attribution: None,
        })
    }
}

#[tokio::test]
async fn mixed_chain_runs_between_legacy_blocks_and_threads_forward_after() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "call", "mixed_tool", r#"{}"#),
            text_stream(&model.id, "done"),
        ],
    );
    let log = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::empty_builder()
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("mixed_tool", "raw"))
        .with_pre_hook(LegacyPre(Arc::clone(&log)))
        .with_execution_hook(OrderedMixed {
            name: "host",
            log: Arc::clone(&log),
        })
        .with_execution_hook(OrderedMixed {
            name: "workspace",
            log: Arc::clone(&log),
        })
        .with_post_hook(LegacyPost(Arc::clone(&log)))
        .build()
        .expect("runtime");
    let mut agent = runtime.spawn("agent", model).expect("agent");

    agent
        .send(vec![ContentBlock::text("go")])
        .await
        .expect("run");

    assert_eq!(
        *log.lock().expect("log"),
        [
            "legacy-pre",
            "host-before",
            "workspace-before",
            "host-after",
            "workspace-after",
            "legacy-post",
        ]
    );
    assert_eq!(
        result_for(agent.history(), "call"),
        ("raw-host-workspace-legacy".into(), false)
    );
}

struct ModifiesBefore {
    input_json: &'static str,
    attribution: &'static str,
    after_inputs: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl ExecutionHookParticipant for ModifiesBefore {
    fn name(&self) -> &str {
        "rewrite"
    }

    async fn before(&self, _context: &PreExecutionContext) -> Result<BeforeDecision, RuntimeError> {
        Ok(BeforeDecision::Modify {
            input_json: self.input_json.to_string(),
            attribution: Some(self.attribution.to_string()),
        })
    }

    async fn after(&self, context: &PostExecutionContext) -> Result<AfterDecision, RuntimeError> {
        self.after_inputs
            .lock()
            .expect("after inputs")
            .push(context.input_json.clone());
        Ok(AfterDecision::Continue)
    }
}

#[tokio::test]
async fn mixed_before_modification_reaches_validation_execution_and_after_context() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "modified",
                "modified_tool",
                r#"{"value":"original"}"#,
            ),
            text_stream(&model.id, "done"),
        ],
    );
    let after_inputs = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::empty_builder()
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("modified_tool", "ok"))
        .with_execution_hook(ModifiesBefore {
            input_json: r#"{"value":"mixed"}"#,
            attribution: "normalized value",
            after_inputs: Arc::clone(&after_inputs),
        })
        .build()
        .expect("runtime");
    let mut agent = runtime.spawn("agent", model).expect("agent");

    agent
        .send(vec![ContentBlock::text("go")])
        .await
        .expect("run");

    assert_eq!(
        *after_inputs.lock().expect("after inputs"),
        [r#"{"value":"mixed"}"#.to_string()]
    );
    assert_eq!(
        result_for(agent.history(), "modified"),
        ("ok".into(), false)
    );
}

#[tokio::test]
async fn mixed_schema_refusal_keeps_attribution_skips_mixed_after_and_keeps_legacy_post() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "invalid", "invalid_tool", r#"{"value":"ok"}"#),
            text_stream(&model.id, "done"),
        ],
    );
    let after_inputs = Arc::new(Mutex::new(Vec::new()));
    let legacy_log = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::empty_builder()
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("invalid_tool", "must not run"))
        .with_execution_hook(ModifiesBefore {
            input_json: r#"{"value":42}"#,
            attribution: "forced number",
            after_inputs: Arc::clone(&after_inputs),
        })
        .with_post_hook(LegacyPost(Arc::clone(&legacy_log)))
        .build()
        .expect("runtime");
    let mut agent = runtime.spawn("agent", model).expect("agent");

    agent
        .send(vec![ContentBlock::text("go")])
        .await
        .expect("run");

    assert!(after_inputs.lock().expect("after inputs").is_empty());
    assert_eq!(*legacy_log.lock().expect("legacy log"), ["legacy-post"]);
    let (result, is_error) = result_for(agent.history(), "invalid");
    assert!(is_error);
    assert!(
        result.starts_with("Blocked by mixed execution hook:"),
        "{result}"
    );
    assert!(
        result.contains("execution hook 'rewrite': forced number"),
        "{result}"
    );
    assert!(result.ends_with("-legacy"), "{result}");
}

struct BlockingParticipant {
    name: &'static str,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    before_calls: Arc<AtomicUsize>,
    after_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ExecutionHookParticipant for BlockingParticipant {
    fn name(&self) -> &str {
        self.name
    }

    async fn before(&self, _context: &PreExecutionContext) -> Result<BeforeDecision, RuntimeError> {
        self.before_calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.release.notified().await;
        Ok(BeforeDecision::Continue)
    }

    async fn after(&self, _context: &PostExecutionContext) -> Result<AfterDecision, RuntimeError> {
        self.after_calls.fetch_add(1, Ordering::SeqCst);
        Ok(AfterDecision::Continue)
    }
}

struct Counts {
    name: &'static str,
    before: Arc<AtomicUsize>,
    after: Arc<AtomicUsize>,
}

#[async_trait]
impl ExecutionHookParticipant for Counts {
    fn name(&self) -> &str {
        self.name
    }

    async fn before(&self, _context: &PreExecutionContext) -> Result<BeforeDecision, RuntimeError> {
        self.before.fetch_add(1, Ordering::SeqCst);
        Ok(BeforeDecision::Continue)
    }

    async fn after(&self, _context: &PostExecutionContext) -> Result<AfterDecision, RuntimeError> {
        self.after.fetch_add(1, Ordering::SeqCst);
        Ok(AfterDecision::Continue)
    }
}

#[tokio::test]
async fn one_serial_snapshot_survives_drop_and_excludes_late_registration() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "first", "snapshot_tool", r#"{}"#),
            text_stream(&model.id, "first done"),
            tool_use_stream(&model.id, "second", "snapshot_tool", r#"{}"#),
            text_stream(&model.id, "second done"),
        ],
    );
    let runtime = Runtime::empty_builder()
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("snapshot_tool", "ok"))
        .build()
        .expect("runtime");
    let audience = ToolAudience::new("workspace");
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let old_before = Arc::new(AtomicUsize::new(0));
    let old_after = Arc::new(AtomicUsize::new(0));
    let guard = runtime.register_execution_hook_for_audience(
        audience.clone(),
        BlockingParticipant {
            name: "old",
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            before_calls: Arc::clone(&old_before),
            after_calls: Arc::clone(&old_after),
        },
    );
    let agent = runtime
        .spawn_with_config_for_audience("agent", model, AgentConfig::default(), audience.clone())
        .expect("agent");
    let running = tokio::spawn(async move {
        let mut agent = agent;
        agent
            .send(vec![ContentBlock::text("first")])
            .await
            .expect("first run");
        agent
    });

    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("old before enters");
    drop(guard);
    let late_before = Arc::new(AtomicUsize::new(0));
    let late_after = Arc::new(AtomicUsize::new(0));
    let _late = runtime.register_execution_hook_for_audience(
        audience,
        Counts {
            name: "late",
            before: Arc::clone(&late_before),
            after: Arc::clone(&late_after),
        },
    );
    release.notify_one();
    let mut agent = tokio::time::timeout(Duration::from_secs(2), running)
        .await
        .expect("first run finishes")
        .expect("run task joins");

    assert_eq!(old_before.load(Ordering::SeqCst), 1);
    assert_eq!(old_after.load(Ordering::SeqCst), 1);
    assert_eq!(late_before.load(Ordering::SeqCst), 0);
    assert_eq!(late_after.load(Ordering::SeqCst), 0);

    agent
        .send(vec![ContentBlock::text("second")])
        .await
        .expect("second run");
    assert_eq!(old_before.load(Ordering::SeqCst), 1);
    assert_eq!(old_after.load(Ordering::SeqCst), 1);
    assert_eq!(late_before.load(Ordering::SeqCst), 1);
    assert_eq!(late_after.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_existing_scoped_session_observes_a_late_mixed_batch() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "session", "session_tool", r#"{}"#),
            text_stream(&model.id, "done"),
        ],
    );
    let runtime = Runtime::empty_builder()
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("session_tool", "ok"))
        .build()
        .expect("runtime");
    let audience = ToolAudience::new("session-workspace");
    let mut session = runtime
        .create_session_with_options(
            "session",
            model,
            SessionOptions {
                policy: Some(RuntimePolicy::default()),
                tool_audience: Some(audience.clone()),
                ..Default::default()
            },
        )
        .expect("session");
    let before = Arc::new(AtomicUsize::new(0));
    let after = Arc::new(AtomicUsize::new(0));
    let participant: Arc<dyn ExecutionHookParticipant> = Arc::new(Counts {
        name: "late",
        before: Arc::clone(&before),
        after: Arc::clone(&after),
    });
    let _registration = runtime.register_execution_hooks_for_audience(audience, [participant]);

    session
        .append_turn(vec![ContentBlock::text("go")])
        .await
        .expect("run");

    assert_eq!(before.load(Ordering::SeqCst), 1);
    assert_eq!(after.load(Ordering::SeqCst), 1);
}

struct DeniesAfter(Arc<AtomicUsize>);

#[async_trait]
impl ExecutionHookParticipant for DeniesAfter {
    fn name(&self) -> &str {
        "guard"
    }

    async fn after(&self, _context: &PostExecutionContext) -> Result<AfterDecision, RuntimeError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(AfterDecision::Deny("secret output".into()))
    }
}

#[tokio::test]
async fn mixed_after_denial_skips_legacy_post_and_becomes_error_result() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "deny", "deny_tool", r#"{}"#),
            text_stream(&model.id, "done"),
        ],
    );
    let legacy_log = Arc::new(Mutex::new(Vec::new()));
    let denied = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::empty_builder()
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("deny_tool", "secret"))
        .with_execution_hook(DeniesAfter(Arc::clone(&denied)))
        .with_post_hook(LegacyPost(Arc::clone(&legacy_log)))
        .build()
        .expect("runtime");
    let mut agent = runtime.spawn("agent", model).expect("agent");

    agent
        .send(vec![ContentBlock::text("go")])
        .await
        .expect("run");

    assert_eq!(denied.load(Ordering::SeqCst), 1);
    assert!(legacy_log.lock().expect("log").is_empty());
    assert_eq!(
        result_for(agent.history(), "deny"),
        (
            "denied by execution hook 'guard': secret output".into(),
            true
        )
    );
}

fn parallel_stream(model: &str) -> super::support::StreamScript {
    ok_stream(vec![
        ProviderEvent::MessageStarted {
            id: "parallel".into(),
            model: model.into(),
            role: Role::Assistant,
        },
        ProviderEvent::ContentBlockStarted {
            index: 0,
            kind: ContentBlockStart::ToolUse {
                id: "p1".into(),
                name: "parallel_one".into(),
            },
        },
        ProviderEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::ToolUseInputJson("{}".into()),
        },
        ProviderEvent::ContentBlockStopped { index: 0 },
        ProviderEvent::ContentBlockStarted {
            index: 1,
            kind: ContentBlockStart::ToolUse {
                id: "p2".into(),
                name: "parallel_two".into(),
            },
        },
        ProviderEvent::ContentBlockDelta {
            index: 1,
            delta: ContentBlockDelta::ToolUseInputJson("{}".into()),
        },
        ProviderEvent::ContentBlockStopped { index: 1 },
        ProviderEvent::MessageStopped,
    ])
}

#[tokio::test]
async fn parallel_executions_retain_their_admitted_snapshot_until_after() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![parallel_stream(&model.id), text_stream(&model.id, "done")],
    );
    let tool_log = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::empty_builder()
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .with_tool(ProbeTool::new(
            "parallel_one",
            true,
            Duration::from_millis(80),
            Arc::clone(&tool_log),
            Arc::clone(&active),
            Arc::clone(&max_active),
        ))
        .with_tool(ProbeTool::new(
            "parallel_two",
            true,
            Duration::from_millis(80),
            Arc::clone(&tool_log),
            Arc::clone(&active),
            Arc::clone(&max_active),
        ))
        .build()
        .expect("runtime");
    let before = Arc::new(AtomicUsize::new(0));
    let after = Arc::new(AtomicUsize::new(0));
    let guard = runtime.register_execution_hook(Counts {
        name: "original",
        before: Arc::clone(&before),
        after: Arc::clone(&after),
    });
    let agent = runtime.spawn("agent", model).expect("agent");
    let running = tokio::spawn(async move {
        let mut agent = agent;
        agent
            .send(vec![ContentBlock::text("go")])
            .await
            .expect("run");
        agent
    });

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if tool_log.lock().await.len() >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both tools start");
    drop(guard);
    let late_before = Arc::new(AtomicUsize::new(0));
    let late_after = Arc::new(AtomicUsize::new(0));
    let _late = runtime.register_execution_hook(Counts {
        name: "late",
        before: Arc::clone(&late_before),
        after: Arc::clone(&late_after),
    });
    let agent = running.await.expect("join");

    assert_eq!(max_active.load(Ordering::SeqCst), 2);
    assert_eq!(before.load(Ordering::SeqCst), 2);
    assert_eq!(after.load(Ordering::SeqCst), 2);
    assert_eq!(late_before.load(Ordering::SeqCst), 0);
    assert_eq!(late_after.load(Ordering::SeqCst), 0);
    assert!(!result_for(agent.history(), "p1").1);
    assert!(!result_for(agent.history(), "p2").1);
}
