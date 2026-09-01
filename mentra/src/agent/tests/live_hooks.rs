use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;

use crate::{
    AgentConfig, BuiltinProvider, ContentBlock, Message, Runtime, ToolAudience,
    agent::WorkspaceConfig,
    error::RuntimeError,
    runtime::{
        HookDecision, PostExecutionContext, PostExecutionHook, PreExecutionContext,
        PreExecutionHook, ResultDecision, RuntimePolicy, SessionOptions, VolatileRuntimeStore,
    },
    tool::ToolResultContent,
};

use super::support::{ScriptedProvider, StaticTool, model_info, text_stream, tool_use_stream};

fn result_for(history: &[Message], call_id: &str) -> String {
    history
        .iter()
        .flat_map(|message| message.content.iter())
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == call_id => Some(content.to_display_string()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing tool result for {call_id}"))
}

struct SuffixPostHook {
    suffix: &'static str,
    calls: Arc<AtomicUsize>,
}

struct CountingPreHook(Arc<AtomicUsize>);

#[async_trait]
impl PreExecutionHook for CountingPreHook {
    async fn pre_tool_execution(
        &self,
        _context: &PreExecutionContext,
    ) -> Result<HookDecision, RuntimeError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(HookDecision::Allow)
    }
}

#[async_trait]
impl PostExecutionHook for SuffixPostHook {
    async fn post_tool_execution(
        &self,
        context: &PostExecutionContext,
    ) -> Result<ResultDecision, RuntimeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ResultDecision::Replace {
            content: ToolResultContent::text(format!(
                "{}{}",
                context.content.to_display_string(),
                self.suffix
            )),
            is_error: context.is_error,
        })
    }
}

#[tokio::test]
async fn existing_agent_and_session_observe_scoped_hooks_until_guard_drop() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "agent-live", "live_hook_tool", r#"{}"#),
            text_stream(&model.id, "agent live done"),
            tool_use_stream(&model.id, "session-live", "live_hook_tool", r#"{}"#),
            text_stream(&model.id, "session live done"),
            tool_use_stream(&model.id, "agent-after", "live_hook_tool", r#"{}"#),
            text_stream(&model.id, "agent after done"),
            tool_use_stream(&model.id, "session-after", "live_hook_tool", r#"{}"#),
            text_stream(&model.id, "session after done"),
        ],
    );
    let runtime = Runtime::empty_builder()
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("live_hook_tool", "raw"))
        .build()
        .expect("build runtime");
    let audience = ToolAudience::new("existing-workspace");
    let mut agent = runtime
        .spawn_with_config_for_audience(
            "existing-agent",
            model.clone(),
            AgentConfig::default(),
            audience.clone(),
        )
        .expect("spawn existing agent");
    let mut session = runtime
        .create_session_with_options(
            "existing-session",
            model,
            SessionOptions {
                policy: Some(RuntimePolicy::default()),
                tool_audience: Some(audience.clone()),
                ..Default::default()
            },
        )
        .expect("create existing session");

    // This is deliberately the only post hook. It pins the audience-aware
    // empty fast path as well as the shared live registry on existing handles.
    let pre_calls = Arc::new(AtomicUsize::new(0));
    let pre_registration = runtime
        .register_pre_hook_for_audience(audience.clone(), CountingPreHook(Arc::clone(&pre_calls)));
    let post_calls = Arc::new(AtomicUsize::new(0));
    let post_registration = runtime.register_post_hook_for_audience(
        audience,
        SuffixPostHook {
            suffix: "-live",
            calls: Arc::clone(&post_calls),
        },
    );
    agent
        .send(vec![ContentBlock::text("agent live")])
        .await
        .expect("agent live turn");
    session
        .append_turn(vec![ContentBlock::text("session live")])
        .await
        .expect("session live turn");
    assert_eq!(result_for(agent.history(), "agent-live"), "raw-live");
    assert_eq!(result_for(session.history(), "session-live"), "raw-live");
    assert_eq!(pre_calls.load(Ordering::SeqCst), 2);
    assert_eq!(post_calls.load(Ordering::SeqCst), 2);

    drop(pre_registration);
    drop(post_registration);
    agent
        .send(vec![ContentBlock::text("agent after")])
        .await
        .expect("agent after turn");
    session
        .append_turn(vec![ContentBlock::text("session after")])
        .await
        .expect("session after turn");
    assert_eq!(result_for(agent.history(), "agent-after"), "raw");
    assert_eq!(result_for(session.history(), "session-after"), "raw");
    assert_eq!(pre_calls.load(Ordering::SeqCst), 2);
    assert_eq!(post_calls.load(Ordering::SeqCst), 2);
}

struct OrderHook {
    label: &'static str,
    log: Arc<Mutex<Vec<(String, &'static str)>>>,
}

impl OrderHook {
    fn record(&self, agent_id: &str) {
        self.log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((agent_id.to_string(), self.label));
    }
}

#[async_trait]
impl PreExecutionHook for OrderHook {
    async fn pre_tool_execution(
        &self,
        context: &PreExecutionContext,
    ) -> Result<HookDecision, RuntimeError> {
        self.record(&context.agent_id);
        Ok(HookDecision::Allow)
    }
}

#[async_trait]
impl PostExecutionHook for OrderHook {
    async fn post_tool_execution(
        &self,
        context: &PostExecutionContext,
    ) -> Result<ResultDecision, RuntimeError> {
        self.record(&context.agent_id);
        Ok(ResultDecision::Keep)
    }
}

fn same_workspace() -> AgentConfig {
    AgentConfig {
        workspace: WorkspaceConfig {
            base_dir: PathBuf::from("/same/workspace/root"),
            auto_route_shell: false,
        },
        ..Default::default()
    }
}

#[tokio::test]
async fn same_root_audiences_filter_one_combined_pre_and_post_order() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "alpha-call", "ordered_hook_tool", r#"{}"#),
            text_stream(&model.id, "alpha done"),
            tool_use_stream(&model.id, "beta-call", "ordered_hook_tool", r#"{}"#),
            text_stream(&model.id, "beta done"),
            tool_use_stream(&model.id, "plain-call", "ordered_hook_tool", r#"{}"#),
            text_stream(&model.id, "plain done"),
        ],
    );
    let log = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::empty_builder()
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("ordered_hook_tool", "raw"))
        .with_pre_hook(OrderHook {
            label: "builder-pre",
            log: Arc::clone(&log),
        })
        .with_post_hook(OrderHook {
            label: "builder-post",
            log: Arc::clone(&log),
        })
        .build()
        .expect("build runtime");
    let alpha = ToolAudience::new("alpha");
    let beta = ToolAudience::new("beta");
    let mut alpha_agent = runtime
        .spawn_with_config_for_audience("alpha", model.clone(), same_workspace(), alpha.clone())
        .expect("alpha agent");
    let mut beta_agent = runtime
        .spawn_with_config_for_audience("beta", model.clone(), same_workspace(), beta.clone())
        .expect("beta agent");
    let mut plain_agent = runtime
        .spawn_with_config("plain", model, same_workspace())
        .expect("plain agent");
    let alpha_id = alpha_agent.id().to_string();
    let beta_id = beta_agent.id().to_string();
    let plain_id = plain_agent.id().to_string();

    let _alpha_pre = runtime.register_pre_hook_for_audience(
        alpha.clone(),
        OrderHook {
            label: "alpha-pre",
            log: Arc::clone(&log),
        },
    );
    let _global_pre = runtime.register_pre_hook(OrderHook {
        label: "global-pre",
        log: Arc::clone(&log),
    });
    let _beta_pre = runtime.register_pre_hook_for_audience(
        beta.clone(),
        OrderHook {
            label: "beta-pre",
            log: Arc::clone(&log),
        },
    );
    let _alpha_post = runtime.register_post_hook_for_audience(
        alpha,
        OrderHook {
            label: "alpha-post",
            log: Arc::clone(&log),
        },
    );
    let _global_post = runtime.register_post_hook(OrderHook {
        label: "global-post",
        log: Arc::clone(&log),
    });
    let _beta_post = runtime.register_post_hook_for_audience(
        beta,
        OrderHook {
            label: "beta-post",
            log: Arc::clone(&log),
        },
    );

    alpha_agent
        .send(vec![ContentBlock::text("alpha")])
        .await
        .expect("alpha turn");
    beta_agent
        .send(vec![ContentBlock::text("beta")])
        .await
        .expect("beta turn");
    plain_agent
        .send(vec![ContentBlock::text("plain")])
        .await
        .expect("plain turn");

    let entries = log
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(
        entries,
        vec![
            (alpha_id.clone(), "builder-pre"),
            (alpha_id.clone(), "alpha-pre"),
            (alpha_id.clone(), "global-pre"),
            (alpha_id.clone(), "global-post"),
            (alpha_id.clone(), "alpha-post"),
            (alpha_id, "builder-post"),
            (beta_id.clone(), "builder-pre"),
            (beta_id.clone(), "global-pre"),
            (beta_id.clone(), "beta-pre"),
            (beta_id.clone(), "beta-post"),
            (beta_id.clone(), "global-post"),
            (beta_id, "builder-post"),
            (plain_id.clone(), "builder-pre"),
            (plain_id.clone(), "global-pre"),
            (plain_id.clone(), "global-post"),
            (plain_id, "builder-post"),
        ]
    );
}
