//! Integration coverage for the volatile, no-durable-trace `RuntimeStore`
//! profile (`VolatileRuntimeStore`) against a full `Agent::run` (via
//! `Agent::send`), not just the store's own unit tests.

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    BuiltinProvider, ContentBlock, Role,
    agent::{AgentConfig, CompactionConfig, TaskConfig, TeamConfig},
    memory::MemoryStore,
    provider::{ContentBlockDelta, ContentBlockStart, ProviderEvent},
    runtime::{AgentStore, Runtime, RuntimePolicy, TaskStore, VolatileRuntimeStore},
};

use super::support::{ScriptedProvider, StaticTool, model_info, ok_stream};

#[tokio::test]
async fn volatile_run_leaves_no_durable_trace_on_disk() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let tasks_dir = temp_path("volatile-notrace-tasks");
    let team_dir = temp_path("volatile-notrace-team");
    let transcript_dir = temp_path("volatile-notrace-transcripts");
    let store = VolatileRuntimeStore::new();

    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            task_tool_stream("tool-1", "task_create", r#"{"subject":"write the report"}"#),
            text_stream("report task created"),
        ],
    );

    let runtime = Runtime::builder()
        .with_store(store.clone())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let config = volatile_config(tasks_dir.clone(), team_dir.clone(), transcript_dir.clone());
    let mut agent = runtime
        .spawn_with_config("primary", model.clone(), config)
        .expect("spawn agent");

    agent
        .send(vec![ContentBlock::Text {
            text: "start".to_string(),
        }])
        .await
        .expect("run completes");
    let agent_id = agent.id().to_string();

    // Give the detached post-run memory-ingest task a chance to run before
    // asserting on the filesystem — it must not create anything either.
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(
        !tasks_dir.exists(),
        "tasks_dir must never be created by the volatile profile"
    );
    assert!(
        !team_dir.exists(),
        "team_dir must never be created by the volatile profile"
    );
    assert!(
        !transcript_dir.exists(),
        "transcript_dir must never be created by the volatile profile"
    );

    // The run's effects are real, just in-memory: the tool call landed in
    // the retained store, and ingest wrote the episode into it too.
    assert_eq!(
        store
            .load_tasks(&tasks_dir)
            .expect("load tasks")
            .into_iter()
            .map(|task| task.subject)
            .collect::<Vec<_>>(),
        vec!["write the report".to_string()]
    );
    assert!(
        !store
            .search_records(&agent_id, "report", 10)
            .expect("search ingested memory")
            .is_empty(),
        "the detached memory-ingest task should have written into the volatile store"
    );
}

#[tokio::test]
async fn volatile_store_truncates_without_creating_spill_artifacts() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let tasks_dir = temp_path("volatile-truncation-tasks");
    let team_dir = temp_path("volatile-truncation-team");
    let transcript_dir = temp_path("volatile-truncation-transcripts");
    let spill_dir = transcript_dir.join("tool-output");
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            task_tool_stream("tool-output", "oversized_output", r#"{}"#),
            text_stream("done"),
        ],
    );

    let runtime = Runtime::empty_builder()
        .with_store(VolatileRuntimeStore::new())
        .with_provider_instance(provider)
        .with_policy(
            RuntimePolicy::default()
                .with_max_tool_result_bytes(usize::MAX)
                .with_max_tool_result_lines(1),
        )
        .with_tool(StaticTool::success("oversized_output", "one\ntwo\nthree"))
        .build()
        .expect("build runtime");
    let config = volatile_config(tasks_dir.clone(), team_dir.clone(), transcript_dir.clone());
    let mut agent = runtime
        .spawn_with_config("primary", model, config)
        .expect("spawn agent");

    agent
        .send(vec![ContentBlock::Text {
            text: "run the oversized tool".to_string(),
        }])
        .await
        .expect("run completes");

    let content = match agent.history()[2].content.first().expect("tool result") {
        ContentBlock::ToolResult {
            content, is_error, ..
        } => {
            assert!(!is_error);
            content.to_display_string()
        }
        other => panic!("unexpected content block: {other:?}"),
    };
    let tasks_dir_exists = tasks_dir.exists();
    let team_dir_exists = team_dir.exists();
    let transcript_dir_exists = transcript_dir.exists();
    let spill_dir_exists = spill_dir.exists();

    for path in [&tasks_dir, &team_dir, &transcript_dir] {
        if path.exists() {
            fs::remove_dir_all(path).expect("remove unexpected volatile artifact directory");
        }
    }

    assert_eq!(
        content,
        "one\n[truncated: showing 1 of 3 lines; full output was not saved because the runtime store forbids durable artifacts]"
    );
    assert!(!tasks_dir_exists, "volatile task artifacts must not exist");
    assert!(!team_dir_exists, "volatile team artifacts must not exist");
    assert!(
        !transcript_dir_exists,
        "volatile transcript artifacts must not exist"
    );
    assert!(!spill_dir_exists, "volatile spill artifacts must not exist");
}

#[tokio::test]
async fn sequential_runs_on_retained_store_do_not_leak_records() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let tasks_dir = temp_path("volatile-isolation-tasks");
    let team_dir = temp_path("volatile-isolation-team");
    let transcript_dir = temp_path("volatile-isolation-transcripts");
    let store = VolatileRuntimeStore::new();
    let config = volatile_config(tasks_dir.clone(), team_dir.clone(), transcript_dir.clone());

    // --- Run 1: same team_dir/tasks_dir/agent name as run 2 below, which is
    // exactly the shared-default scenario the volatile profile's isolation
    // contract has to defend against on a retained store. ---
    let provider_1 = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            task_tool_stream("tool-1", "task_create", r#"{"subject":"first run task"}"#),
            text_stream("first run complete"),
        ],
    );
    let runtime_1 = Runtime::builder()
        .with_store(store.clone())
        .with_provider_instance(provider_1)
        .build()
        .expect("build runtime 1");
    let mut agent_1 = runtime_1
        .spawn_with_config("primary", model.clone(), config.clone())
        .expect("spawn agent 1");
    agent_1
        .send(vec![ContentBlock::Text {
            text: "go".to_string(),
        }])
        .await
        .expect("run 1 completes");
    let agent_1_id = agent_1.id().to_string();
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        store.list_agents().expect("list agents after run 1").len(),
        1
    );
    assert_eq!(
        store
            .load_tasks(&tasks_dir)
            .expect("tasks after run 1")
            .len(),
        1
    );

    // Explicit isolation seam: reset the retained store between runs.
    store.reset();

    // --- Run 2: a fresh agent (fresh id, same name/dirs) must observe none
    // of run 1's records through the same retained store instance. ---
    let provider_2 = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![text_stream("second run complete")],
    );
    let runtime_2 = Runtime::builder()
        .with_store(store.clone())
        .with_provider_instance(provider_2)
        .build()
        .expect("build runtime 2");
    let mut agent_2 = runtime_2
        .spawn_with_config("primary", model.clone(), config)
        .expect("spawn agent 2");
    agent_2
        .send(vec![ContentBlock::Text {
            text: "go".to_string(),
        }])
        .await
        .expect("run 2 completes");
    let agent_2_id = agent_2.id().to_string();
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_ne!(agent_1_id, agent_2_id, "each spawn gets a fresh agent id");

    let agents_after_run_2 = store.list_agents().expect("list agents after run 2");
    assert_eq!(
        agents_after_run_2.len(),
        1,
        "run 2 must not see run 1's agent record"
    );
    assert_eq!(agents_after_run_2[0].record.id, agent_2_id);

    assert!(
        store
            .load_tasks(&tasks_dir)
            .expect("tasks after run 2")
            .is_empty(),
        "run 2 must not see run 1's task, which was written under the same tasks_dir"
    );

    assert!(
        store
            .search_records(&agent_1_id, "first run", 10)
            .expect("search for agent 1's memory by its own id")
            .is_empty(),
        "agent 1's own record disappeared with reset(), so nothing can match its id"
    );
    assert!(
        !store
            .search_records(&agent_2_id, "second run", 10)
            .expect("search for agent 2's memory")
            .is_empty(),
        "run 2's own ingested memory is still visible to itself"
    );
}

#[tokio::test]
async fn retained_store_without_reset_shares_state_across_runs() {
    // Companion to `sequential_runs_on_retained_store_do_not_leak_records`:
    // demonstrates that `reset()` is doing real work by showing what happens
    // without it. A retained `VolatileRuntimeStore` is a shared database —
    // exactly like two runs pointed at the same `SqliteRuntimeStore` path —
    // when the host does not call `reset()` between runs.
    let model = model_info("model", BuiltinProvider::Anthropic);
    let tasks_dir = temp_path("volatile-shared-tasks");
    let team_dir = temp_path("volatile-shared-team");
    let transcript_dir = temp_path("volatile-shared-transcripts");
    let store = VolatileRuntimeStore::new();
    let config = volatile_config(tasks_dir.clone(), team_dir.clone(), transcript_dir.clone());

    let provider_1 = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            task_tool_stream("tool-1", "task_create", r#"{"subject":"first run task"}"#),
            text_stream("first run complete"),
        ],
    );
    let runtime_1 = Runtime::builder()
        .with_store(store.clone())
        .with_provider_instance(provider_1)
        .build()
        .expect("build runtime 1");
    let mut agent_1 = runtime_1
        .spawn_with_config("primary", model.clone(), config.clone())
        .expect("spawn agent 1");
    agent_1
        .send(vec![ContentBlock::Text {
            text: "go".to_string(),
        }])
        .await
        .expect("run 1 completes");

    // No reset() here.

    let provider_2 = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![text_stream("second run complete")],
    );
    let runtime_2 = Runtime::builder()
        .with_store(store.clone())
        .with_provider_instance(provider_2)
        .build()
        .expect("build runtime 2");
    let mut agent_2 = runtime_2
        .spawn_with_config("primary", model.clone(), config)
        .expect("spawn agent 2");
    agent_2
        .send(vec![ContentBlock::Text {
            text: "go".to_string(),
        }])
        .await
        .expect("run 2 completes");

    assert_eq!(
        store.list_agents().expect("list agents").len(),
        2,
        "without reset(), both agents' records remain in the shared store"
    );
    assert_eq!(
        store
            .load_tasks(&tasks_dir)
            .expect("tasks after both runs")
            .len(),
        1,
        "without reset(), run 1's task is still visible under the shared tasks_dir"
    );
}

fn volatile_config(tasks_dir: PathBuf, team_dir: PathBuf, transcript_dir: PathBuf) -> AgentConfig {
    AgentConfig {
        task: TaskConfig {
            tasks_dir,
            reminder_threshold: 3,
        },
        team: TeamConfig {
            team_dir,
            ..Default::default()
        },
        compaction: CompactionConfig {
            transcript_dir,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn task_tool_stream(
    tool_id: &str,
    tool_name: &str,
    input_json: &str,
) -> super::support::StreamScript {
    ok_stream(vec![
        ProviderEvent::MessageStarted {
            id: format!("msg-{tool_id}"),
            model: "model".to_string(),
            role: Role::Assistant,
        },
        ProviderEvent::ContentBlockStarted {
            index: 0,
            kind: ContentBlockStart::ToolUse {
                id: tool_id.to_string(),
                name: tool_name.to_string(),
            },
        },
        ProviderEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::ToolUseInputJson(input_json.to_string()),
        },
        ProviderEvent::ContentBlockStopped { index: 0 },
        ProviderEvent::MessageStopped,
    ])
}

fn text_stream(text: &str) -> super::support::StreamScript {
    ok_stream(vec![
        ProviderEvent::MessageStarted {
            id: format!("msg-{text}"),
            model: "model".to_string(),
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
    ])
}

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

/// Builds a unique path under the system temp directory *without* creating
/// it — the whole point of these tests is to assert the volatile profile
/// never creates it either.
fn temp_path(label: &str) -> PathBuf {
    let unique = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    std::env::temp_dir().join(format!("mentra-{label}-{timestamp}-{unique}"))
}
