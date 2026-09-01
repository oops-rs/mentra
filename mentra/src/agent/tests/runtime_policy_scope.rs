use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::json;

use crate::{
    AgentConfig, BuiltinProvider, ContentBlock, FileToolProfile, Message, RuntimePolicy,
    runtime::{Runtime, SessionOptions, SessionResumeOptions, VolatileRuntimeStore},
    session::{Session, SessionEvent, TaskLifecycleStatus},
};

use super::support::{
    ScriptedProvider, StaticTool, model_info, shell_pwd_command, text_stream, tool_use_stream,
};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "mentra-session-policy-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

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
        .unwrap_or_else(|| panic!("missing tool result for {call_id}"))
}

fn workspace_config(path: &Path) -> AgentConfig {
    AgentConfig {
        workspace: crate::agent::WorkspaceConfig {
            base_dir: path.to_path_buf(),
            ..Default::default()
        },
        ..Default::default()
    }
}

async fn append_turn(session: &mut Session, prompt: &str) {
    session
        .append_turn(vec![ContentBlock::text(prompt)])
        .await
        .expect("scripted turn succeeds");
}

#[tokio::test]
async fn contradictory_session_policies_are_isolated_and_none_inherits_the_runtime() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let command = json!({ "command": shell_pwd_command() }).to_string();
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "allowed-shell", "shell", &command),
            text_stream(&model.id, "allowed done"),
            tool_use_stream(&model.id, "denied-shell", "shell", &command),
            text_stream(&model.id, "denied done"),
            tool_use_stream(&model.id, "inherited-shell", "shell", &command),
            text_stream(&model.id, "inherited done"),
        ],
    );
    let runtime = Runtime::builder()
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .with_policy(RuntimePolicy::default())
        .build()
        .expect("build runtime");

    let mut allowed = runtime
        .create_session_with_options(
            "allowed",
            model.clone(),
            SessionOptions {
                policy: Some(RuntimePolicy::permissive()),
                ..Default::default()
            },
        )
        .expect("create allowed session");
    let mut denied = runtime
        .create_session_with_options(
            "denied",
            model.clone(),
            SessionOptions {
                policy: Some(RuntimePolicy::default()),
                ..Default::default()
            },
        )
        .expect("create denied session");
    let mut inherited = runtime
        .create_session("inherited", model)
        .expect("create inherited session");

    append_turn(&mut allowed, "run the shell").await;
    append_turn(&mut denied, "run the shell").await;
    append_turn(&mut inherited, "run the shell").await;

    let (_, allowed_error) = result_for(allowed.history(), "allowed-shell");
    let (denied_result, denied_error) = result_for(denied.history(), "denied-shell");
    let (inherited_result, inherited_error) = result_for(inherited.history(), "inherited-shell");
    assert!(
        !allowed_error,
        "the permissive session executes its command"
    );
    assert!(denied_error, "the scoped default policy denies its command");
    assert!(
        denied_result.contains("disabled by the runtime policy"),
        "{denied_result}"
    );
    assert!(
        inherited_error && inherited_result.contains("disabled by the runtime policy"),
        "None must inherit the runtime policy: {inherited_result}"
    );
}

async fn assert_protected_write_is_denied(
    profile: FileToolProfile,
    tool_name: &str,
    call_id: &str,
    input: serde_json::Value,
    relative_target: &Path,
) {
    let directory = TestDirectory::new(tool_name);
    let protected_root = directory.path().join(".git").join("hooks");
    fs::create_dir_all(&protected_root).expect("create protected root");
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, call_id, tool_name, &input.to_string()),
            text_stream(&model.id, "done"),
        ],
    );
    let runtime = Runtime::builder()
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .with_file_tools(profile)
        .with_policy(RuntimePolicy::permissive())
        .build()
        .expect("build runtime");
    let policy =
        RuntimePolicy::workspace_bounded(directory.path()).with_denied_write_root(protected_root);
    let mut session = runtime
        .create_session_with_options(
            "protected",
            model,
            SessionOptions {
                config: workspace_config(directory.path()),
                policy: Some(policy),
                ..Default::default()
            },
        )
        .expect("create protected session");

    append_turn(&mut session, "write the protected file").await;

    let (result, is_error) = result_for(session.history(), call_id);
    assert!(is_error, "the protected write must fail: {result}");
    assert!(result.contains("denied write root"), "{result}");
    assert!(
        !directory.path().join(relative_target).exists(),
        "the denied write must not reach the filesystem"
    );
}

#[tokio::test]
async fn batched_and_split_file_writes_use_the_session_policy() {
    assert_protected_write_is_denied(
        FileToolProfile::Batched,
        "files",
        "batched-write",
        json!({
            "operations": [{
                "op": "create",
                "path": ".git/hooks/pre-commit",
                "content": "echo denied"
            }]
        }),
        Path::new(".git/hooks/pre-commit"),
    )
    .await;

    assert_protected_write_is_denied(
        FileToolProfile::Split,
        "write",
        "split-write",
        json!({
            "path": ".git/hooks/pre-push",
            "content": "echo denied"
        }),
        Path::new(".git/hooks/pre-push"),
    )
    .await;
}

#[tokio::test]
async fn tool_result_caps_use_the_session_policy() {
    const FULL_OUTPUT: &str = "abcdefghijklmnopqrstuvwxyz0123456789";

    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "capped-output", "long_output", r#"{}"#),
            text_stream(&model.id, "done"),
        ],
    );
    let runtime = Runtime::empty_builder()
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("long_output", FULL_OUTPUT))
        .with_policy(
            RuntimePolicy::permissive()
                .with_max_tool_result_bytes(usize::MAX)
                .with_max_tool_result_lines(usize::MAX),
        )
        .build()
        .expect("build runtime");
    let policy = RuntimePolicy::permissive()
        .with_max_tool_result_bytes(16)
        .with_max_tool_result_lines(usize::MAX)
        .spill_full_tool_output(false);
    let mut session = runtime
        .create_session_with_options(
            "capped",
            model,
            SessionOptions {
                policy: Some(policy),
                ..Default::default()
            },
        )
        .expect("create capped session");

    append_turn(&mut session, "return the long output").await;

    let (result, is_error) = result_for(session.history(), "capped-output");
    assert!(!is_error, "the tool itself succeeds: {result}");
    assert!(result.contains("[truncated:"), "{result}");
    assert!(
        !result.contains(FULL_OUTPUT),
        "the full body must be capped"
    );
}

#[tokio::test]
async fn disposable_subagents_inherit_the_session_policy() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let command = json!({ "command": shell_pwd_command() }).to_string();
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "child-shell", "shell", &command),
            text_stream(&model.id, "child done"),
        ],
    );
    let provider_log = provider.clone();
    let runtime = Runtime::builder()
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .with_policy(RuntimePolicy::permissive())
        .build()
        .expect("build runtime");
    let mut session = runtime
        .create_session_with_options(
            "parent",
            model,
            SessionOptions {
                policy: Some(RuntimePolicy::default()),
                ..Default::default()
            },
        )
        .expect("create parent session");
    let mut events = session.subscribe();
    let subagent = session
        .spawn_subagent("child", "try the shell")
        .await
        .expect("spawn child");

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await.expect("subagent event") {
                SessionEvent::TaskUpdated {
                    task_id,
                    status: TaskLifecycleStatus::Finished,
                    ..
                } if task_id == subagent.task_id => break,
                _ => continue,
            }
        }
    })
    .await
    .expect("subagent finishes");

    let requests = provider_log.recorded_requests().await;
    assert_eq!(requests.len(), 2);
    let (result, is_error) = result_for(&requests[1].messages, "child-shell");
    assert!(is_error, "the child must inherit the denial: {result}");
    assert!(
        result.contains("disabled by the runtime policy"),
        "{result}"
    );
}

#[tokio::test]
async fn resume_uses_the_supplied_current_policy_and_never_persists_it() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let command = json!({ "command": shell_pwd_command() }).to_string();
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "resumed-allowed", "shell", &command),
            text_stream(&model.id, "allowed done"),
            tool_use_stream(&model.id, "resumed-inherited", "shell", &command),
            text_stream(&model.id, "inherited done"),
        ],
    );
    let runtime = Runtime::builder()
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .with_policy(RuntimePolicy::default())
        .build()
        .expect("build runtime");
    let original = runtime
        .create_session_with_options(
            "persisted",
            model,
            SessionOptions {
                policy: Some(RuntimePolicy::permissive()),
                ..Default::default()
            },
        )
        .expect("create persisted session");
    let agent_id = original.agent_id().to_string();
    drop(original);

    let mut explicitly_permissive = runtime
        .resume_session_with_options(
            &agent_id,
            SessionResumeOptions {
                policy: Some(RuntimePolicy::permissive()),
                ..Default::default()
            },
        )
        .expect("resume with current policy");
    append_turn(&mut explicitly_permissive, "run after explicit resume").await;
    let (_, allowed_error) = result_for(explicitly_permissive.history(), "resumed-allowed");
    assert!(!allowed_error, "the supplied current policy must apply");
    drop(explicitly_permissive);

    let mut inherited = runtime
        .resume_session(&agent_id)
        .expect("resume with runtime policy");
    append_turn(&mut inherited, "run after inherited resume").await;
    let (result, is_error) = result_for(inherited.history(), "resumed-inherited");
    assert!(is_error, "the old scoped policy must not be persisted");
    assert!(
        result.contains("disabled by the runtime policy"),
        "{result}"
    );
}
