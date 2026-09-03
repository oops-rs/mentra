use std::{
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{
    ContentBlock,
    error::RuntimeError,
    runtime::{CancellationToken, RunOptions},
    runtime::{FileRuntimeStore, PermissionRuleStore, RuntimeStore, VolatileRuntimeStore},
    session::{
        PermissionDecision, PermissionRuleAddress, PermissionRuleScope, RememberedRule, RuleKey,
        SessionEvent, SessionPermissionHandle,
    },
    test::{MockRuntime, MockToolCall},
    tool::{
        ParallelToolContext, ToolAuthorizationDecision, ToolAuthorizationRequest, ToolAuthorizer,
        ToolDefinition, ToolExecutor, ToolResult, ToolSpec,
    },
};

const PROBE_TOOL: &str = "permission_store_probe";

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos()
}

fn rule(scope: PermissionRuleScope, allow: bool) -> RememberedRule {
    RememberedRule {
        key: RuleKey {
            tool_name: PROBE_TOOL.to_owned(),
            pattern: None,
        },
        allow,
        scope,
        reason: (!allow).then(|| format!("{scope:?} refuses")),
    }
}

fn address(scope: PermissionRuleScope) -> PermissionRuleAddress {
    PermissionRuleAddress {
        scope,
        key: RuleKey {
            tool_name: PROBE_TOOL.to_owned(),
            pattern: None,
        },
    }
}

fn permission_handle(agent_id: &str, store: Arc<dyn RuntimeStore>) -> SessionPermissionHandle {
    let (event_tx, _) = tokio::sync::broadcast::channel(8);
    SessionPermissionHandle::new(
        agent_id.to_owned(),
        None,
        store,
        event_tx,
        crate::session::permission::PendingPermissionStore::new(),
    )
}

#[test]
fn process_rules_belong_to_one_in_process_session_binding() {
    let store: Arc<dyn RuntimeStore> = Arc::new(VolatileRuntimeStore::new());
    let first = permission_handle("same-agent", Arc::clone(&store));
    let first_clone = first.clone();
    let independent = permission_handle("same-agent", Arc::clone(&store));

    first
        .remember_rule(rule(PermissionRuleScope::Process, true))
        .expect("remember process rule");

    assert_eq!(
        first_clone
            .matching_rule(PROBE_TOOL, None)
            .expect("match cloned handle")
            .expect("cloned handle shares the live binding")
            .scope,
        PermissionRuleScope::Process
    );
    assert!(
        independent
            .matching_rule(PROBE_TOOL, None)
            .expect("match independent handle")
            .is_none(),
        "the stable agent id must not make process rules durable or shared"
    );

    drop(first);
    drop(first_clone);
    let replacement = permission_handle("same-agent", store);
    assert!(
        replacement
            .matching_rule(PROBE_TOOL, None)
            .expect("match replacement handle")
            .is_none(),
        "a new live binding must start empty after the old one is dropped"
    );
}

#[test]
fn process_rules_precede_every_durable_scope() {
    let store = VolatileRuntimeStore::new();
    let mock = MockRuntime::builder()
        .with_store(store)
        .build()
        .expect("build runtime");
    let session = mock
        .runtime()
        .create_session_full(
            "precedence",
            mock.model(),
            Default::default(),
            Some("project-1".to_owned()),
        )
        .expect("create session");
    let permissions = session.permission_handle();

    for scope in [
        PermissionRuleScope::Global,
        PermissionRuleScope::Project,
        PermissionRuleScope::Session,
    ] {
        permissions
            .remember_rule(rule(scope, false))
            .expect("remember durable denial");
    }
    permissions
        .remember_rule(rule(PermissionRuleScope::Process, true))
        .expect("remember process allow");

    let matched = permissions
        .matching_rule(PROBE_TOOL, None)
        .expect("match rule")
        .expect("a rule should match");
    assert_eq!(matched.scope, PermissionRuleScope::Process);
    assert!(matched.allow);
    assert_eq!(
        permissions
            .remembered_rules()
            .expect("list effective rules")
            .into_iter()
            .map(|rule| rule.scope)
            .collect::<Vec<_>>(),
        vec![
            PermissionRuleScope::Process,
            PermissionRuleScope::Session,
            PermissionRuleScope::Project,
            PermissionRuleScope::Global,
        ]
    );

    assert_eq!(
        permissions
            .clear_scope(PermissionRuleScope::Process)
            .expect("clear process rules"),
        1
    );
    let fallback = permissions
        .matching_rule(PROBE_TOOL, None)
        .expect("match after process clear")
        .expect("durable session rule should remain");
    assert_eq!(fallback.scope, PermissionRuleScope::Session);
    assert!(!fallback.allow);

    let process_rule = rule(PermissionRuleScope::Process, true);
    let process_address = PermissionRuleAddress::from(&process_rule);
    permissions
        .remember_rule(process_rule)
        .expect("remember replacement process allow");
    assert!(
        permissions
            .revoke_rule(&process_address)
            .expect("revoke process rule")
    );
    assert_eq!(
        permissions
            .matching_rule(PROBE_TOOL, None)
            .expect("match after process revoke")
            .expect("durable session rule should remain")
            .scope,
        PermissionRuleScope::Session
    );
}

#[test]
fn process_resolution_never_reads_or_writes_the_durable_backend() {
    let root = std::env::temp_dir().join(format!(
        "mentra-process-permission-{}-{}",
        std::process::id(),
        now_nanos()
    ));
    let store = FileRuntimeStore::new(&root);
    let mock = MockRuntime::builder()
        .with_store(store.clone())
        .build()
        .expect("build runtime");
    let session = mock
        .runtime()
        .create_session("non-persisted", mock.model())
        .expect("create session");
    let permissions = session.permission_handle();
    let mut events = session.subscribe();
    let (sender, mut receiver) = tokio::sync::oneshot::channel();
    let _guard = permissions.pending_permissions().insert(
        "perm-live-session".to_owned(),
        crate::session::permission::PendingPermissionEntry {
            tool_call_id: "call-live-session".to_owned(),
            tool_name: PROBE_TOOL.to_owned(),
            sender,
        },
    );
    let rules_path = store.rules_path();
    assert!(!rules_path.exists());
    std::fs::write(&rules_path, b"{").expect("install corrupt durable-rule sentinel");

    permissions
        .resolve_permission(
            "perm-live-session",
            PermissionDecision::allow_and_remember(PermissionRuleScope::Process),
        )
        .expect("resolve with a process rule");
    assert!(receiver.try_recv().expect("receive decision").allow);
    assert!(matches!(
        events.try_recv().expect("permission resolved event"),
        SessionEvent::PermissionResolved {
            rule_scope: Some(PermissionRuleScope::Process),
            ..
        }
    ));
    assert_eq!(
        std::fs::read(&rules_path).expect("read durable-rule sentinel after resolution"),
        b"{",
        "resolving a process answer must neither read nor rewrite durable state"
    );

    let matched = permissions
        .matching_rule(PROBE_TOOL, None)
        .expect("the live rule must answer before reading durable state")
        .expect("live rule should match");
    assert_eq!(matched.scope, PermissionRuleScope::Process);
    assert_eq!(
        std::fs::read(&rules_path).expect("read durable-rule sentinel"),
        b"{",
        "matching a live rule must not rewrite durable state"
    );
}

#[test]
fn process_rules_do_not_survive_resume() {
    let store = VolatileRuntimeStore::new();
    let mock = MockRuntime::builder()
        .with_store(store)
        .build()
        .expect("build runtime");
    let session = mock
        .runtime()
        .create_session("live-lifetime", mock.model())
        .expect("create session");
    let agent_id = session.agent_id().to_owned();
    session
        .permission_handle()
        .remember_rule(rule(PermissionRuleScope::Process, true))
        .expect("remember process rule");
    assert_eq!(
        session.remembered_rules().expect("list live rules").len(),
        1
    );
    drop(session);

    let resumed = mock
        .runtime()
        .resume_session(&agent_id)
        .expect("resume persisted agent");
    assert!(
        resumed
            .remembered_rules()
            .expect("list resumed rules")
            .is_empty(),
        "resuming the same stable agent must create a fresh process rung"
    );
}

#[test]
fn live_sessions_share_project_and_global_mutations_without_a_cache() {
    let store = VolatileRuntimeStore::new();
    let mock = MockRuntime::builder()
        .with_store(store)
        .build()
        .expect("build runtime");
    let session_a = mock
        .runtime()
        .create_session_full(
            "a",
            mock.model(),
            Default::default(),
            Some("project-1".to_owned()),
        )
        .expect("create session a");
    let session_b = mock
        .runtime()
        .create_session_full(
            "b",
            mock.model(),
            Default::default(),
            Some("project-1".to_owned()),
        )
        .expect("create session b");
    let session_c = mock
        .runtime()
        .create_session_full(
            "c",
            mock.model(),
            Default::default(),
            Some("project-2".to_owned()),
        )
        .expect("create session c");
    let permissions_a = session_a.permission_handle();
    let permissions_b = session_b.permission_handle();
    let permissions_c = session_c.permission_handle();

    permissions_a
        .remember_rule(rule(PermissionRuleScope::Global, false))
        .expect("remember global rule");
    permissions_a
        .remember_rule(rule(PermissionRuleScope::Project, false))
        .expect("remember project rule");
    permissions_a
        .remember_rule(rule(PermissionRuleScope::Session, true))
        .expect("remember session rule");

    assert_eq!(
        permissions_a
            .matching_rule(PROBE_TOOL, None)
            .expect("match session a")
            .expect("session a rule")
            .scope,
        PermissionRuleScope::Session
    );
    assert_eq!(
        permissions_b
            .matching_rule(PROBE_TOOL, None)
            .expect("match session b")
            .expect("session b rule")
            .scope,
        PermissionRuleScope::Project
    );
    assert_eq!(
        permissions_c
            .matching_rule(PROBE_TOOL, None)
            .expect("match session c")
            .expect("session c rule")
            .scope,
        PermissionRuleScope::Global
    );

    assert!(
        permissions_b
            .revoke_rule(&address(PermissionRuleScope::Project))
            .expect("revoke shared project rule")
    );
    assert_eq!(
        permissions_a
            .matching_rule(PROBE_TOOL, None)
            .expect("match session a after project revoke")
            .expect("session a keeps its own rule")
            .scope,
        PermissionRuleScope::Session
    );
    assert_eq!(
        permissions_b
            .matching_rule(PROBE_TOOL, None)
            .expect("match session b after project revoke")
            .expect("session b falls back to global")
            .scope,
        PermissionRuleScope::Global
    );

    assert_eq!(
        permissions_c
            .clear_scope(PermissionRuleScope::Global)
            .expect("clear global namespace"),
        1
    );
    assert!(
        permissions_b
            .matching_rule(PROBE_TOOL, None)
            .expect("match session b after global clear")
            .is_none()
    );
}

#[tokio::test]
async fn project_validation_keeps_the_pending_request_available() {
    let mock = MockRuntime::builder().build().expect("build runtime");
    let session = mock
        .runtime()
        .create_session("no-project", mock.model())
        .expect("create session");
    let permissions = session.permission_handle();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let _guard = permissions.pending_permissions().insert(
        "perm-project".to_owned(),
        crate::session::permission::PendingPermissionEntry {
            tool_call_id: "call-project".to_owned(),
            tool_name: PROBE_TOOL.to_owned(),
            sender,
        },
    );

    let error = permissions
        .resolve_permission(
            "perm-project",
            PermissionDecision::allow_and_remember(PermissionRuleScope::Project),
        )
        .expect_err("project scope requires a project id");
    assert!(error.to_string().contains("project_id"), "{error}");
    assert!(permissions.pending_permissions().contains("perm-project"));
    assert!(permissions.remembered_rules().unwrap().is_empty());

    permissions
        .resolve_permission("perm-project", PermissionDecision::deny())
        .expect("the retained request can still be resolved");
    assert!(!receiver.await.expect("receive retained decision").allow);
}

#[tokio::test]
async fn a_persistence_failure_keeps_the_pending_request_retryable() {
    let root = std::env::temp_dir().join(format!(
        "mentra-live-permission-resolve-failure-{}-{}",
        std::process::id(),
        now_nanos()
    ));
    let store = FileRuntimeStore::new(&root);
    let mock = MockRuntime::builder()
        .with_store(store.clone())
        .build()
        .unwrap();
    let session = mock
        .runtime()
        .create_session("resolve-failure", mock.model())
        .unwrap();
    let permissions = session.permission_handle();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let _guard = permissions.pending_permissions().insert(
        "perm-retry".to_owned(),
        crate::session::permission::PendingPermissionEntry {
            tool_call_id: "call-retry".to_owned(),
            tool_name: PROBE_TOOL.to_owned(),
            sender,
        },
    );
    std::fs::write(store.rules_path(), b"{").expect("corrupt rules file");

    permissions
        .resolve_permission(
            "perm-retry",
            PermissionDecision::allow_and_remember(PermissionRuleScope::Session),
        )
        .expect_err("the failed durable write must be reported");
    assert!(permissions.pending_permissions().contains("perm-retry"));
    assert!(receiver.is_empty());

    std::fs::remove_file(store.rules_path()).expect("remove corrupt rules file");
    permissions
        .resolve_permission(
            "perm-retry",
            PermissionDecision::allow_and_remember(PermissionRuleScope::Session),
        )
        .expect("retry durable write");
    assert!(receiver.await.expect("receive retried decision").allow);
    assert_eq!(permissions.remembered_rules().unwrap().len(), 1);
}

#[test]
fn resolve_racing_wait_drop_never_persists_after_a_failed_send() {
    let mock = MockRuntime::builder().build().unwrap();
    let session = mock
        .runtime()
        .create_session("resolve-race", mock.model())
        .unwrap();
    let permissions = session.permission_handle();

    for iteration in 0..64 {
        let request_id = format!("perm-race-{iteration}");
        let tool_name = format!("race-tool-{iteration}");
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let guard = permissions.pending_permissions().insert(
            request_id.clone(),
            crate::session::permission::PendingPermissionEntry {
                tool_call_id: format!("call-race-{iteration}"),
                tool_name: tool_name.clone(),
                sender,
            },
        );
        let barrier = Arc::new(Barrier::new(2));
        let drop_barrier = barrier.clone();
        let dropper = thread::spawn(move || {
            drop_barrier.wait();
            drop(guard);
            drop(receiver);
        });

        barrier.wait();
        let result = permissions.resolve_permission(
            &request_id,
            PermissionDecision::allow_and_remember(PermissionRuleScope::Session),
        );
        dropper.join().expect("drop thread should finish");
        let persisted = permissions
            .remembered_rules()
            .unwrap()
            .iter()
            .any(|rule| rule.key.tool_name == tool_name);
        assert_eq!(
            persisted,
            result.is_ok(),
            "iteration {iteration}: persistence and live delivery must have one winner"
        );
    }
}

#[test]
fn remembering_one_session_rule_never_copies_inherited_rules() {
    let store = VolatileRuntimeStore::new();
    let mock = MockRuntime::builder()
        .with_store(store.clone())
        .build()
        .expect("build runtime");
    let session_a = mock
        .runtime()
        .create_session_full(
            "a",
            mock.model(),
            Default::default(),
            Some("project-1".to_owned()),
        )
        .expect("create session a");
    let session_b = mock
        .runtime()
        .create_session_full(
            "b",
            mock.model(),
            Default::default(),
            Some("project-1".to_owned()),
        )
        .expect("create session b");
    session_a
        .permission_handle()
        .remember_rule(rule(PermissionRuleScope::Project, false))
        .unwrap();
    session_a
        .permission_handle()
        .remember_rule(RememberedRule {
            key: RuleKey {
                tool_name: "network".to_owned(),
                pattern: None,
            },
            allow: false,
            scope: PermissionRuleScope::Global,
            reason: None,
        })
        .unwrap();
    let permissions_b = session_b.permission_handle();
    let (sender, _receiver) = tokio::sync::oneshot::channel();
    let _guard = permissions_b.pending_permissions().insert(
        "perm-session-b".to_owned(),
        crate::session::permission::PendingPermissionEntry {
            tool_call_id: "call-session-b".to_owned(),
            tool_name: PROBE_TOOL.to_owned(),
            sender,
        },
    );
    permissions_b
        .resolve_permission(
            "perm-session-b",
            PermissionDecision::allow_and_remember(PermissionRuleScope::Session),
        )
        .unwrap();

    let context = permissions_b.context().clone();
    assert_eq!(
        store
            .clear_scope(&context, PermissionRuleScope::Project)
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .clear_scope(&context, PermissionRuleScope::Global)
            .unwrap(),
        1
    );
}

async fn assert_session_rule_survives_and_revocation_reopens_empty<S, F>(make_store: F, label: &str)
where
    S: RuntimeStore + Clone + 'static,
    F: Fn() -> S,
{
    let runtime_id = format!("permission-live-{label}");
    let agent_id;
    {
        let mock = MockRuntime::builder()
            .runtime_identifier(&runtime_id)
            .with_store(make_store())
            .build()
            .unwrap();
        let session = mock
            .runtime()
            .create_session("persisted", mock.model())
            .unwrap();
        agent_id = session.agent_id().to_owned();
        session
            .permission_handle()
            .remember_rule(rule(PermissionRuleScope::Session, true))
            .unwrap();
    }

    {
        let mock = MockRuntime::builder()
            .runtime_identifier(&runtime_id)
            .with_store(make_store())
            .build()
            .unwrap();
        let resumed = mock.runtime().resume_session(&agent_id).unwrap();
        assert_eq!(resumed.permission_handle().context().session_id, agent_id);
        assert_eq!(resumed.remembered_rules().unwrap().len(), 1);
        assert!(
            resumed
                .permission_handle()
                .revoke_rule(&address(PermissionRuleScope::Session))
                .unwrap()
        );
    }

    let mock = MockRuntime::builder()
        .runtime_identifier(runtime_id)
        .with_store(make_store())
        .build()
        .unwrap();
    let resumed = mock.runtime().resume_session(&agent_id).unwrap();
    assert!(resumed.remembered_rules().unwrap().is_empty());
}

#[tokio::test]
async fn file_store_session_rules_survive_resume_and_revocation_survives_reopen() {
    let root = std::env::temp_dir().join(format!(
        "mentra-live-permission-file-{}-{}",
        std::process::id(),
        now_nanos()
    ));
    assert_session_rule_survives_and_revocation_reopens_empty(
        || FileRuntimeStore::new(&root),
        "file",
    )
    .await;
}

#[cfg(feature = "store-sqlite")]
#[tokio::test]
async fn sqlite_session_rules_survive_resume_and_revocation_survives_reopen() {
    let path = std::env::temp_dir().join(format!(
        "mentra-live-permission-sqlite-{}.sqlite",
        now_nanos()
    ));
    assert_session_rule_survives_and_revocation_reopens_empty(
        || crate::runtime::SqliteRuntimeStore::new(&path),
        "sqlite",
    )
    .await;
}

#[derive(Clone)]
struct PromptThenDeny(Arc<AtomicBool>);

impl PromptThenDeny {
    fn prompting() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    fn deny(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl ToolAuthorizer for PromptThenDeny {
    async fn authorize(
        &self,
        _request: &ToolAuthorizationRequest,
    ) -> Result<ToolAuthorizationDecision, RuntimeError> {
        Ok(if self.0.load(Ordering::SeqCst) {
            ToolAuthorizationDecision::deny("current mode refuses")
        } else {
            ToolAuthorizationDecision::prompt("current mode asks")
        })
    }
}

#[derive(Clone)]
struct ProbeTool(Arc<AtomicUsize>);

impl ToolDefinition for ProbeTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder(PROBE_TOOL)
            .description("count permission-store executions")
            .input_schema(json!({ "type": "object" }))
            .build()
    }
}

#[async_trait]
impl ToolExecutor for ProbeTool {
    async fn execute(&self, _ctx: ParallelToolContext, _input: Value) -> ToolResult {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok("executed".to_owned())
    }
}

#[tokio::test]
async fn clearing_session_rules_and_switching_mode_governs_the_next_call() {
    let policy = PromptThenDeny::prompting();
    let mock = MockRuntime::builder()
        .tool_calls([MockToolCall::new(PROBE_TOOL, json!({}))])
        .text("first")
        .tool_calls([MockToolCall::new(PROBE_TOOL, json!({}))])
        .text("second")
        .build()
        .unwrap();
    let executions = Arc::new(AtomicUsize::new(0));
    mock.runtime().register_tool(ProbeTool(executions.clone()));
    let mut session = mock
        .runtime()
        .create_session("mode", mock.model())
        .unwrap()
        .with_tool_authorizer(policy.clone());
    session
        .permission_handle()
        .remember_rule(rule(PermissionRuleScope::Session, true))
        .unwrap();

    session
        .append_turn(vec![ContentBlock::text("first")])
        .await
        .unwrap();
    assert_eq!(executions.load(Ordering::SeqCst), 1);

    assert_eq!(
        session
            .permission_handle()
            .clear_scope(PermissionRuleScope::Session)
            .unwrap(),
        1
    );
    policy.deny();
    session
        .append_turn(vec![ContentBlock::text("second")])
        .await
        .unwrap();
    assert_eq!(executions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_live_rule_store_error_fails_closed_without_prompting_or_execution() {
    let root = std::env::temp_dir().join(format!(
        "mentra-live-permission-corrupt-{}-{}",
        std::process::id(),
        now_nanos()
    ));
    let store = FileRuntimeStore::new(&root);
    let policy = PromptThenDeny::prompting();
    let mock = MockRuntime::builder()
        .with_store(store.clone())
        .tool_calls([MockToolCall::new(PROBE_TOOL, json!({}))])
        .text("done")
        .build()
        .unwrap();
    let executions = Arc::new(AtomicUsize::new(0));
    mock.runtime().register_tool(ProbeTool(executions.clone()));
    let mut session = mock
        .runtime()
        .create_session("corrupt-store", mock.model())
        .unwrap()
        .with_tool_authorizer(policy);
    let mut events = session.subscribe();
    std::fs::write(store.rules_path(), b"{").expect("corrupt rules file");

    session
        .append_turn(vec![ContentBlock::text("run")])
        .await
        .expect("authorizer failure is returned to the model as a denied tool result");

    assert_eq!(executions.load(Ordering::SeqCst), 0);
    let mut requested = false;
    while let Ok(event) = events.try_recv() {
        requested |= matches!(event, SessionEvent::PermissionRequested { .. });
    }
    assert!(!requested, "a failed store lookup must not ask or execute");
}

#[tokio::test]
async fn cancelling_a_permission_wait_removes_pending_and_rejects_late_rules() {
    let policy = PromptThenDeny::prompting();
    let mock = MockRuntime::builder()
        .tool_calls([MockToolCall::new(PROBE_TOOL, json!({}))])
        .text("never reached")
        .build()
        .unwrap();
    mock.runtime()
        .register_tool(ProbeTool(Arc::new(AtomicUsize::new(0))));
    let mut session = mock
        .runtime()
        .create_session("cancel-permission", mock.model())
        .unwrap()
        .with_tool_authorizer(policy);
    let permissions = session.permission_handle();
    let mut events = session.subscribe();
    let cancellation = CancellationToken::default();
    let cancellation_for_turn = cancellation.clone();
    let turn = tokio::spawn(async move {
        session
            .append_turn_with_options(
                vec![ContentBlock::text("run")],
                RunOptions {
                    cancellation: Some(cancellation_for_turn),
                    ..RunOptions::default()
                },
            )
            .await
    });
    let request_id = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let SessionEvent::PermissionRequested { request_id, .. } = events
                .recv()
                .await
                .expect("session event stream remains open")
            {
                break request_id;
            }
        }
    })
    .await
    .expect("permission request should arrive");

    cancellation.cancel();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), turn)
            .await
            .expect("cancelled turn should finish")
            .expect("turn task should join")
            .is_err()
    );
    assert!(!permissions.pending_permissions().contains(&request_id));
    assert!(
        permissions
            .resolve_permission(
                &request_id,
                PermissionDecision::allow_and_remember(PermissionRuleScope::Global),
            )
            .is_err()
    );
    assert!(permissions.remembered_rules().unwrap().is_empty());
}
