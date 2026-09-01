use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::Value;

use crate::{
    AgentConfig, BuiltinProvider, ContentBlock, Runtime, ToolAudience,
    agent::ToolProfile,
    error::RuntimeError,
    runtime::{
        SessionOptions, SessionResumeOptions,
        control::{HookDecision, PreExecutionContext, PreExecutionHook},
    },
    tool::{
        ParallelToolContext, ToolAuthorizationDecision, ToolAuthorizationRequest, ToolAuthorizer,
        ToolDefinition, ToolExecutionCategory, ToolExecutor, ToolResult, ToolSpec,
    },
};

use super::support::{
    PersistentStore, ScriptedProvider, StaticTool, model_info, text_stream, tool_use_stream,
};

fn persistent_runtime(store: PersistentStore, model: &crate::ModelInfo) -> Runtime {
    Runtime::empty_builder()
        .with_store(store)
        .with_provider_instance(ScriptedProvider::new(
            BuiltinProvider::Anthropic,
            vec![model.clone()],
            vec![],
        ))
        .build()
        .expect("build persistent runtime")
}

fn audience_store() -> PersistentStore {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    PersistentStore::new(std::env::temp_dir().join(format!(
        "mentra-tool-audience-{}-{nonce}",
        std::process::id()
    )))
}

struct CountingTool {
    name: &'static str,
    output: &'static str,
    ran: Arc<AtomicUsize>,
}

impl ToolDefinition for CountingTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder(self.name)
            .execution_category(ToolExecutionCategory::ReadOnlyParallel)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for CountingTool {
    fn execution_category(&self, _input: &Value) -> ToolExecutionCategory {
        ToolExecutionCategory::ReadOnlyParallel
    }

    async fn execute(&self, _ctx: ParallelToolContext, _input: Value) -> ToolResult {
        self.ran.fetch_add(1, Ordering::SeqCst);
        Ok(self.output.to_string())
    }
}

struct LayeredTool {
    label: &'static str,
}

impl ToolDefinition for LayeredTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("layered_tool")
            .description(self.label)
            .execution_category(ToolExecutionCategory::ReadOnlyParallel)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for LayeredTool {
    fn execution_category(&self, _input: &Value) -> ToolExecutionCategory {
        ToolExecutionCategory::ReadOnlyParallel
    }

    async fn execute(&self, _ctx: ParallelToolContext, _input: Value) -> ToolResult {
        Ok(self.label.to_string())
    }
}

struct CountingHook(Arc<AtomicUsize>);

#[async_trait]
impl PreExecutionHook for CountingHook {
    async fn pre_tool_execution(
        &self,
        _context: &PreExecutionContext,
    ) -> Result<HookDecision, RuntimeError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(HookDecision::Allow)
    }
}

struct CountingAuthorizer(Arc<AtomicUsize>);

#[async_trait]
impl ToolAuthorizer for CountingAuthorizer {
    async fn authorize(
        &self,
        _request: &ToolAuthorizationRequest,
    ) -> Result<ToolAuthorizationDecision, RuntimeError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolAuthorizationDecision::allow())
    }
}

fn result_for(agent: &crate::agent::Agent, call_id: &str) -> String {
    agent
        .history()
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
        .unwrap_or_else(|| panic!("missing result for {call_id}"))
}

fn tool_names(request: &crate::provider::Request<'_>) -> Vec<String> {
    request.tools.iter().map(|tool| tool.name.clone()).collect()
}

#[tokio::test]
async fn two_audiences_resolve_same_name_to_different_handlers_and_keep_globals() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "alpha-call", "audience_tool", r#"{}"#),
            text_stream(&model.id, "alpha done"),
            tool_use_stream(&model.id, "beta-call", "audience_tool", r#"{}"#),
            text_stream(&model.id, "beta done"),
        ],
    );
    let provider_handle = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let alpha = ToolAudience::new("alpha");
    let beta = ToolAudience::new("beta");
    let _alpha_guard = runtime
        .try_register_tool_for_audience(
            alpha.clone(),
            StaticTool::success("audience_tool", "alpha output"),
        )
        .expect("alpha tool");
    let _beta_guard = runtime
        .try_register_tool_for_audience(
            beta.clone(),
            StaticTool::success("audience_tool", "beta output"),
        )
        .expect("beta tool");
    runtime.register_tool(StaticTool::success("global_tool", "global"));
    let mut alpha_agent = runtime
        .spawn_with_config_for_audience(
            "alpha",
            model.clone(),
            AgentConfig::default(),
            alpha.clone(),
        )
        .expect("alpha agent");
    let mut beta_agent = runtime
        .spawn_with_config_for_audience("beta", model, AgentConfig::default(), beta.clone())
        .expect("beta agent");

    assert_eq!(alpha_agent.tool_audience(), Some(&alpha));
    assert_eq!(beta_agent.tool_audience(), Some(&beta));
    alpha_agent
        .send(vec![ContentBlock::text("go")])
        .await
        .expect("alpha run");
    beta_agent
        .send(vec![ContentBlock::text("go")])
        .await
        .expect("beta run");

    assert_eq!(result_for(&alpha_agent, "alpha-call"), "alpha output");
    assert_eq!(result_for(&beta_agent, "beta-call"), "beta output");
    let requests = provider_handle.recorded_requests().await;
    for request in [&requests[0], &requests[2]] {
        let names = tool_names(request);
        assert!(names.contains(&"audience_tool".to_string()));
        assert!(names.contains(&"global_tool".to_string()));
    }
}

#[tokio::test]
async fn roster_order_and_same_name_precedence_are_stable() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "exact-call", "layered_tool", r#"{}"#),
            text_stream(&model.id, "exact done"),
            tool_use_stream(&model.id, "audience-call", "layered_tool", r#"{}"#),
            text_stream(&model.id, "audience done"),
            tool_use_stream(&model.id, "audience-fallback-call", "layered_tool", r#"{}"#),
            text_stream(&model.id, "audience fallback done"),
            tool_use_stream(&model.id, "global-call", "layered_tool", r#"{}"#),
            text_stream(&model.id, "global done"),
        ],
    );
    let provider_handle = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    runtime.register_tool(StaticTool::success("z_global", "z"));
    runtime.register_tool(StaticTool::success("a_global", "a"));
    runtime.register_tool(LayeredTool { label: "global" });
    let audience = ToolAudience::new("layered");
    let mut exact = runtime
        .spawn_with_config_for_audience(
            "exact",
            model.clone(),
            AgentConfig::default(),
            audience.clone(),
        )
        .expect("exact agent");
    let handle = exact.runtime_handle();
    let exact_registration = handle.register_agent_tool(exact.id(), LayeredTool { label: "exact" });
    let prepared = crate::tool::ToolRegistry::prepare_tool(LayeredTool { label: "audience" });
    let displaced = {
        let mut registry = handle
            .tooling
            .tool_registry
            .write()
            .expect("tool registry poisoned");
        registry
            .insert_audience_prepared(&audience, prepared)
            .into_parts()
            .1
    };
    assert!(displaced.is_empty());

    let expected_roster = exact
        .tools()
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<Vec<_>>();
    assert!(expected_roster.windows(2).all(|pair| pair[0] <= pair[1]));
    for _ in 0..32 {
        assert_eq!(
            exact
                .tools()
                .iter()
                .map(|tool| tool.name.clone())
                .collect::<Vec<_>>(),
            expected_roster
        );
    }

    let mut audience_agent = runtime
        .spawn_with_config_for_audience(
            "audience",
            model.clone(),
            AgentConfig::default(),
            audience.clone(),
        )
        .expect("audience agent");
    exact
        .send(vec![ContentBlock::text("go")])
        .await
        .expect("exact run");
    audience_agent
        .send(vec![ContentBlock::text("go")])
        .await
        .expect("audience run");

    let mut audience_fallback = runtime
        .spawn_with_config_for_audience(
            "audience-fallback",
            model.clone(),
            AgentConfig::default(),
            audience,
        )
        .expect("audience fallback agent");
    let mut global = runtime.spawn("global", model).expect("global agent");
    audience_fallback
        .send(vec![ContentBlock::text("go")])
        .await
        .expect("audience fallback run");
    global
        .send(vec![ContentBlock::text("go")])
        .await
        .expect("global run");

    assert_eq!(result_for(&exact, "exact-call"), "exact");
    assert_eq!(result_for(&audience_agent, "audience-call"), "audience");
    assert_eq!(
        result_for(&audience_fallback, "audience-fallback-call"),
        "audience"
    );
    assert_eq!(result_for(&global, "global-call"), "global");
    let requests = provider_handle.recorded_requests().await;
    for (index, expected) in [
        (0, "exact"),
        (2, "audience"),
        (4, "audience"),
        (6, "global"),
    ] {
        assert_eq!(
            requests[index]
                .tools
                .iter()
                .find(|tool| tool.name == "layered_tool")
                .and_then(|tool| tool.description.as_deref()),
            Some(expected)
        );
    }
    assert!(exact_registration.unregister());
}

#[tokio::test]
async fn a_foreign_guessed_tool_stops_before_hooks_authorizer_and_handler() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "foreign-call", "foreign_tool", r#"{}"#),
            text_stream(&model.id, "done"),
        ],
    );
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let authorization_calls = Arc::new(AtomicUsize::new(0));
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_pre_hook(CountingHook(Arc::clone(&hook_calls)))
        .with_tool_authorizer(CountingAuthorizer(Arc::clone(&authorization_calls)))
        .build()
        .expect("build runtime");
    let _guard = runtime
        .try_register_tool_for_audience(
            ToolAudience::new("owner"),
            CountingTool {
                name: "foreign_tool",
                output: "must not run",
                ran: Arc::clone(&tool_calls),
            },
        )
        .expect("owner tool");
    let mut foreign = runtime.spawn("foreign", model).expect("global-only agent");
    assert_eq!(foreign.tool_audience(), None);

    foreign
        .send(vec![ContentBlock::text("guess")])
        .await
        .expect("run");

    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    assert_eq!(hook_calls.load(Ordering::SeqCst), 0);
    assert_eq!(authorization_calls.load(Ordering::SeqCst), 0);
    assert!(result_for(&foreign, "foreign-call").contains("not available"));
}

#[tokio::test]
async fn an_existing_session_observes_late_audience_registration_and_drop() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            text_stream(&model.id, "first"),
            text_stream(&model.id, "second"),
            text_stream(&model.id, "third"),
        ],
    );
    let provider_handle = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    runtime.register_tool(StaticTool::success("global_tool", "global"));
    let audience = ToolAudience::new("session");
    let mut session = runtime
        .create_session_with_options(
            "session",
            model,
            SessionOptions {
                config: AgentConfig::default(),
                tool_audience: Some(audience.clone()),
                project_id: None,
                runtime_identifier: None,
            },
        )
        .expect("create session");
    assert_eq!(session.tool_audience(), Some(&audience));

    session
        .append_turn(vec![ContentBlock::text("first")])
        .await
        .expect("first");
    let guard = runtime
        .try_register_tool_for_audience(audience, StaticTool::success("late_tool", "late"))
        .expect("late registration");
    session
        .append_turn(vec![ContentBlock::text("second")])
        .await
        .expect("second");
    drop(guard);
    session
        .append_turn(vec![ContentBlock::text("third")])
        .await
        .expect("third");

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 3);
    assert!(!tool_names(&requests[0]).contains(&"late_tool".to_string()));
    assert!(tool_names(&requests[1]).contains(&"late_tool".to_string()));
    assert!(!tool_names(&requests[2]).contains(&"late_tool".to_string()));
    assert!(
        requests
            .iter()
            .all(|request| tool_names(request).contains(&"global_tool".to_string()))
    );
}

#[test]
fn resume_audience_is_explicit_ephemeral_and_never_persisted() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let store = audience_store();
    let runtime = persistent_runtime(store.clone(), &model);
    let original_audience = ToolAudience::new("original");
    let agent = runtime
        .spawn_with_config_for_audience(
            "agent",
            model.clone(),
            AgentConfig::default(),
            original_audience.clone(),
        )
        .expect("spawn agent");
    assert_eq!(agent.tool_audience(), Some(&original_audience));
    assert!(
        serde_json::to_value(agent.config())
            .expect("serialize config")
            .get("tool_audience")
            .is_none()
    );
    let agent_id = agent.id().to_string();
    drop(agent);
    drop(runtime);

    let runtime = persistent_runtime(store.clone(), &model);
    let resumed = runtime.resume_agent(&agent_id).expect("global resume");
    assert_eq!(resumed.tool_audience(), None);
    drop(resumed);
    drop(runtime);
    let current = ToolAudience::new("current");
    let runtime = persistent_runtime(store.clone(), &model);
    let resumed = runtime
        .resume_agent_for_audience(&agent_id, current.clone())
        .expect("audience resume");
    assert_eq!(resumed.tool_audience(), Some(&current));
    drop(resumed);
    drop(runtime);

    let bulk_audience = ToolAudience::new("bulk-current");
    let runtime = persistent_runtime(store.clone(), &model);
    let resumed = runtime
        .resume_for_audience("default", bulk_audience.clone())
        .expect("bulk audience resume");
    let resumed_agent = resumed
        .iter()
        .find(|agent| agent.id() == agent_id)
        .expect("bulk resume includes original agent");
    assert_eq!(resumed_agent.tool_audience(), Some(&bulk_audience));
    drop(resumed);
    drop(runtime);

    let runtime = persistent_runtime(store.clone(), &model);
    let session = runtime
        .create_session_with_options(
            "session",
            model.clone(),
            SessionOptions {
                config: AgentConfig::default(),
                tool_audience: Some(original_audience),
                project_id: None,
                runtime_identifier: None,
            },
        )
        .expect("create session");
    let session_agent_id = session.agent_id().to_string();
    drop(session);
    drop(runtime);
    let runtime = persistent_runtime(store.clone(), &model);
    let resumed = runtime
        .resume_session(&session_agent_id)
        .expect("global session resume");
    assert_eq!(resumed.tool_audience(), None);
    drop(resumed);
    drop(runtime);
    let runtime = persistent_runtime(store, &model);
    let resumed = runtime
        .resume_session_with_options(
            &session_agent_id,
            SessionResumeOptions {
                project_id: None,
                tool_audience: Some(current.clone()),
            },
        )
        .expect("audience session resume");
    assert_eq!(resumed.tool_audience(), Some(&current));
}

#[tokio::test]
async fn disposable_subagents_inherit_audience_through_profile_replacement() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(BuiltinProvider::Anthropic, vec![model.clone()], vec![]);
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let audience = ToolAudience::new("delegation");
    let _guard = runtime
        .try_register_tool_for_audience(
            audience.clone(),
            StaticTool::success("delegated_tool", "ok"),
        )
        .expect("audience tool");
    let parent = runtime
        .spawn_with_config_for_audience("parent", model, AgentConfig::default(), audience.clone())
        .expect("parent");

    let plain = parent.spawn_subagent().expect("plain child");
    assert_eq!(plain.tool_audience(), Some(&audience));
    let template = parent
        .disposable_subagent_template()
        .with_tool_profile(ToolProfile::only(["delegated_tool"]));
    let narrowed = parent
        .spawn_subagent_from(template)
        .await
        .expect("narrowed child");
    assert_eq!(narrowed.tool_audience(), Some(&audience));
    assert!(narrowed.can_use_tool("delegated_tool"));
}
