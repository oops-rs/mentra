use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{
    AgentConfig, ContentBlock, ToolAudience,
    error::RuntimeError,
    runtime::{SessionOptions, VolatileRuntimeStore},
    session::{Session, SessionEvent, TaskLifecycleStatus},
    test::{MockRuntime, MockRuntimeBuilder, MockToolCall},
    tool::{
        ParallelToolContext, ToolAuthorizationDecision, ToolAuthorizationRequest, ToolAuthorizer,
        ToolDefinition, ToolExecutor, ToolResult, ToolSpec,
    },
};

const PROBE_TOOL: &str = "scoped_authorizer_probe";
const AUDIENCE_PROBE_TOOL: &str = "scoped_authorizer_audience_probe";

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
enum Decision {
    Allow = 0,
    Deny = 1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SeenAuthorization {
    agent_id: String,
    agent_name: String,
    working_directory: PathBuf,
}

#[derive(Clone)]
struct SwitchAuthorizer {
    decision: Arc<AtomicU8>,
    seen: Arc<Mutex<Vec<SeenAuthorization>>>,
}

impl SwitchAuthorizer {
    fn new(decision: Decision) -> Self {
        Self {
            decision: Arc::new(AtomicU8::new(decision as u8)),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn set(&self, decision: Decision) {
        self.decision.store(decision as u8, Ordering::SeqCst);
    }

    fn seen(&self) -> Vec<SeenAuthorization> {
        self.seen
            .lock()
            .expect("authorization log poisoned")
            .clone()
    }
}

#[async_trait]
impl ToolAuthorizer for SwitchAuthorizer {
    async fn authorize(
        &self,
        request: &ToolAuthorizationRequest,
    ) -> Result<ToolAuthorizationDecision, RuntimeError> {
        self.seen
            .lock()
            .expect("authorization log poisoned")
            .push(SeenAuthorization {
                agent_id: request.agent_id.clone(),
                agent_name: request.agent_name.clone(),
                working_directory: request.preview.working_directory.clone(),
            });

        Ok(match self.decision.load(Ordering::SeqCst) {
            value if value == Decision::Allow as u8 => ToolAuthorizationDecision::allow(),
            _ => ToolAuthorizationDecision::deny("the session refuses"),
        })
    }
}

struct PromptWithTimeout;

#[async_trait]
impl ToolAuthorizer for PromptWithTimeout {
    async fn authorize(
        &self,
        _request: &ToolAuthorizationRequest,
    ) -> Result<ToolAuthorizationDecision, RuntimeError> {
        Ok(ToolAuthorizationDecision::prompt("wait for the session"))
    }

    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_millis(250))
    }
}

#[derive(Clone)]
struct ProbeTool {
    name: &'static str,
    executions: Arc<AtomicUsize>,
}

impl ToolDefinition for ProbeTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder(self.name)
            .description("Record one scoped-authorizer test execution")
            .input_schema(json!({
                "type": "object",
                "properties": {
                    "turn": { "type": "integer" }
                },
                "required": ["turn"]
            }))
            .build()
    }
}

#[async_trait]
impl ToolExecutor for ProbeTool {
    async fn execute(&self, _ctx: ParallelToolContext, _input: Value) -> ToolResult {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok("executed".to_string())
    }
}

fn scripted_builder(tool_calls: usize) -> MockRuntimeBuilder {
    scripted_builder_for_tools((0..tool_calls).map(|_| PROBE_TOOL))
}

fn scripted_builder_for_tools(tools: impl IntoIterator<Item = &'static str>) -> MockRuntimeBuilder {
    let mut builder = MockRuntime::builder();
    for (turn, tool) in tools.into_iter().enumerate() {
        builder = builder
            .tool_calls([MockToolCall::new(tool, json!({ "turn": turn }))])
            .text(format!("finished {turn}"));
    }
    builder
}

fn register_probe(mock: &MockRuntime, executions: &Arc<AtomicUsize>) {
    mock.runtime().register_tool(ProbeTool {
        name: PROBE_TOOL,
        executions: Arc::clone(executions),
    });
}

async fn append_probe_turn(session: &mut Session, turn: usize) {
    let response = session
        .append_turn(vec![ContentBlock::text(format!("run probe {turn}"))])
        .await
        .expect("scripted session turn succeeds");
    assert_eq!(response.text(), format!("finished {turn}"));
}

fn nested_existing_workspace() -> PathBuf {
    let current = std::env::current_dir().expect("current directory");
    let local_src = current.join("src");
    if local_src.is_dir() {
        local_src
    } else {
        let workspace_src = current.join("mentra").join("src");
        assert!(
            workspace_src.is_dir(),
            "expected a nested workspace directory"
        );
        workspace_src
    }
}

#[tokio::test]
async fn a_session_override_isolated_from_siblings_and_the_runtime_default() {
    let runtime_authorizer = SwitchAuthorizer::new(Decision::Deny);
    let session_authorizer = SwitchAuthorizer::new(Decision::Allow);
    let mock = scripted_builder_for_tools([AUDIENCE_PROBE_TOOL, PROBE_TOOL, PROBE_TOOL])
        .with_tool_authorizer(runtime_authorizer.clone())
        .build()
        .expect("build mock runtime");
    let executions = Arc::new(AtomicUsize::new(0));
    register_probe(&mock, &executions);

    let workspace = nested_existing_workspace();
    let mut config = AgentConfig::default();
    config.workspace.base_dir = workspace.clone();
    let audience = ToolAudience::new("scoped-authorizer-workspace");
    let _audience_probe = mock
        .runtime()
        .try_register_tool_for_audience(
            audience.clone(),
            ProbeTool {
                name: AUDIENCE_PROBE_TOOL,
                executions: Arc::clone(&executions),
            },
        )
        .expect("register audience-only probe");
    let mut scoped = mock
        .runtime()
        .create_session_with_options(
            "scoped",
            mock.model(),
            SessionOptions {
                config,
                tool_audience: Some(audience.clone()),
                ..SessionOptions::default()
            },
        )
        .expect("create scoped session")
        .with_tool_authorizer(session_authorizer.clone());
    assert_eq!(scoped.tool_audience(), Some(&audience));

    let mut sibling = mock
        .runtime()
        .create_session("sibling", mock.model())
        .expect("create sibling session");
    let mut direct = mock
        .runtime()
        .spawn("direct", mock.model())
        .expect("spawn direct agent");

    append_probe_turn(&mut scoped, 0).await;
    append_probe_turn(&mut sibling, 1).await;
    direct
        .send(vec![ContentBlock::text("run probe 2")])
        .await
        .expect("direct agent turn succeeds");

    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert_eq!(session_authorizer.seen().len(), 1);
    assert_eq!(runtime_authorizer.seen().len(), 2);
    assert_eq!(
        session_authorizer.seen()[0].working_directory,
        workspace,
        "in-place replacement must retain the registered agent context"
    );
}

#[tokio::test]
async fn a_stateful_session_authorizer_governs_each_next_turn() {
    let runtime_authorizer = SwitchAuthorizer::new(Decision::Deny);
    let session_authorizer = SwitchAuthorizer::new(Decision::Allow);
    let mock = scripted_builder(2)
        .with_tool_authorizer(runtime_authorizer.clone())
        .build()
        .expect("build mock runtime");
    let executions = Arc::new(AtomicUsize::new(0));
    register_probe(&mock, &executions);
    let mut session = mock
        .runtime()
        .create_session("stateful", mock.model())
        .expect("create session")
        .with_tool_authorizer(session_authorizer.clone());

    append_probe_turn(&mut session, 0).await;
    session_authorizer.set(Decision::Deny);
    append_probe_turn(&mut session, 1).await;

    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert_eq!(session_authorizer.seen().len(), 2);
    assert!(runtime_authorizer.seen().is_empty());
}

#[tokio::test]
async fn a_session_override_keeps_its_authorization_timeout() {
    let mock = scripted_builder(1).build().expect("build mock runtime");
    let executions = Arc::new(AtomicUsize::new(0));
    register_probe(&mock, &executions);
    let mut session = mock
        .runtime()
        .create_session("timed", mock.model())
        .expect("create session")
        .with_tool_authorizer(PromptWithTimeout);
    let mut events = session.subscribe();

    let append = tokio::spawn(async move { append_probe_turn(&mut session, 0).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if matches!(
                events
                    .recv()
                    .await
                    .expect("session event stream remains open"),
                SessionEvent::PermissionRequested { .. }
            ) {
                break;
            }
        }
    })
    .await
    .expect("the scoped Prompt must reach the session before its timeout");
    tokio::time::timeout(Duration::from_secs(2), append)
        .await
        .expect("the scoped authorizer timeout must finish the turn")
        .expect("append task succeeds");

    assert_eq!(
        executions.load(Ordering::SeqCst),
        0,
        "the outer session wrapper must forward the replacement timeout"
    );
}

#[tokio::test]
async fn a_disposable_subagent_inherits_the_session_authorizer() {
    let runtime_authorizer = SwitchAuthorizer::new(Decision::Deny);
    let session_authorizer = SwitchAuthorizer::new(Decision::Allow);
    let mock = scripted_builder(1)
        .with_tool_authorizer(runtime_authorizer.clone())
        .build()
        .expect("build mock runtime");
    let executions = Arc::new(AtomicUsize::new(0));
    register_probe(&mock, &executions);
    let mut session = mock
        .runtime()
        .create_session("parent", mock.model())
        .expect("create parent session")
        .with_tool_authorizer(session_authorizer.clone());
    let mut events = session.subscribe();

    let handle = session
        .spawn_subagent("child", "run the probe")
        .await
        .expect("spawn disposable subagent");
    let (status, _) = super::next_subagent_outcome(&mut events).await;

    assert_eq!(status, TaskLifecycleStatus::Finished);
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert!(runtime_authorizer.seen().is_empty());
    let seen = session_authorizer.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].agent_id, handle.agent_id);
    assert!(seen[0].agent_name.ends_with("::task"));
}

#[tokio::test]
async fn a_resumed_session_requires_and_accepts_a_fresh_authorizer() {
    const RUNTIME_ID: &str = "scoped-authorizer-resume";

    let store = VolatileRuntimeStore::new();
    let original_authorizer = SwitchAuthorizer::new(Decision::Allow);
    let first = scripted_builder(0)
        .runtime_identifier(RUNTIME_ID)
        .with_store(store.clone())
        .with_tool_authorizer(SwitchAuthorizer::new(Decision::Deny))
        .build()
        .expect("build first runtime");
    let session = first
        .runtime()
        .create_session("persisted", first.model())
        .expect("create persisted session")
        .with_tool_authorizer(original_authorizer.clone());
    let agent_id = session.agent_id().to_string();
    drop(session);
    drop(first);

    let inherited_runtime_authorizer = SwitchAuthorizer::new(Decision::Deny);
    let without_fresh = scripted_builder(1)
        .runtime_identifier(RUNTIME_ID)
        .with_store(store.clone())
        .with_tool_authorizer(inherited_runtime_authorizer.clone())
        .build()
        .expect("build resume runtime without override");
    let executions = Arc::new(AtomicUsize::new(0));
    register_probe(&without_fresh, &executions);
    let mut resumed = without_fresh
        .runtime()
        .resume_session(&agent_id)
        .expect("resume without fresh authorizer");
    append_probe_turn(&mut resumed, 0).await;
    assert_eq!(executions.load(Ordering::SeqCst), 0);
    assert_eq!(inherited_runtime_authorizer.seen().len(), 1);
    assert!(original_authorizer.seen().is_empty());
    drop(resumed);
    drop(without_fresh);

    let fresh_authorizer = SwitchAuthorizer::new(Decision::Allow);
    let with_fresh = scripted_builder(1)
        .runtime_identifier(RUNTIME_ID)
        .with_store(store)
        .with_tool_authorizer(SwitchAuthorizer::new(Decision::Deny))
        .build()
        .expect("build resume runtime with override");
    register_probe(&with_fresh, &executions);
    let mut resumed = with_fresh
        .runtime()
        .resume_session(&agent_id)
        .expect("resume with fresh authorizer")
        .with_tool_authorizer(fresh_authorizer.clone());
    append_probe_turn(&mut resumed, 0).await;

    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert_eq!(fresh_authorizer.seen().len(), 1);
}
