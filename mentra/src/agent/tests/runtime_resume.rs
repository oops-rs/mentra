use std::{
    fs,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::{Notify, watch};

use crate::{
    BuiltinProvider, ContentBlock, Message, Role,
    agent::{AgentConfig, AgentSnapshot, AgentStatus, TeamConfig},
    provider::{ContentBlockDelta, ContentBlockStart, ProviderEvent},
    runtime::{Runtime, RuntimeStore, SqliteRuntimeStore, TeamMemberStatus},
    tool::{
        ExecutableTool, ToolContext, ToolDurability, ToolResult, ToolSideEffectLevel, ToolSpec,
    },
};

use super::support::{ScriptedProvider, controlled_stream, model_info, ok_stream};

#[tokio::test]
async fn runtime_startup_preserves_memory_until_resume_and_resume_rolls_back_pending_turn() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let store = temp_store("pending-recovery");
    let (script, tx) = controlled_stream();
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![script],
    );

    let runtime = Runtime::empty_builder()
        .with_store(store.clone())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let agent = runtime.spawn("agent", model.clone()).expect("spawn agent");
    let agent_id = agent.id().to_string();
    let mut snapshot = agent.watch_snapshot();

    let send_task = tokio::spawn(async move {
        let mut agent = agent;
        let result = agent
            .send(vec![ContentBlock::Text {
                text: "hello".to_string(),
            }])
            .await;
        (agent, result)
    });

    wait_for_status(&mut snapshot, AgentStatus::Streaming).await;
    tx.send(Ok(ProviderEvent::MessageStarted {
        id: "msg-1".to_string(),
        model: model.id.clone(),
        role: Role::Assistant,
    }))
    .expect("message started");
    snapshot.changed().await.expect("snapshot changed");

    tx.send(Ok(ProviderEvent::ContentBlockStarted {
        index: 0,
        kind: ContentBlockStart::Text,
    }))
    .expect("block started");
    snapshot.changed().await.expect("snapshot changed");

    tx.send(Ok(ProviderEvent::ContentBlockDelta {
        index: 0,
        delta: ContentBlockDelta::Text("Hel".to_string()),
    }))
    .expect("delta");
    snapshot.changed().await.expect("snapshot changed");
    assert_eq!(snapshot.borrow().current_text, "Hel");

    send_task.abort();
    let _ = send_task.await;
    clear_leases(&store);

    let reboot_runtime = Runtime::empty_builder()
        .with_store(store.clone())
        .with_provider_instance(ScriptedProvider::new(
            BuiltinProvider::Anthropic,
            vec![model.clone()],
            Vec::new(),
        ))
        .build()
        .expect("rebuild runtime");

    let persisted_before_resume = store
        .load_agent(&agent_id)
        .expect("load interrupted state")
        .expect("agent state");
    assert_eq!(
        persisted_before_resume
            .memory
            .pending_turn
            .as_ref()
            .expect("pending turn persisted")
            .current_text,
        "Hel"
    );

    let resumed = reboot_runtime
        .resume_agent(&agent_id)
        .expect("resume interrupted agent");
    assert!(resumed.history().is_empty());
    assert_eq!(
        resumed.watch_snapshot().borrow().status,
        AgentStatus::Interrupted
    );
    assert!(resumed.watch_snapshot().borrow().current_text.is_empty());

    let persisted_after_resume = store
        .load_agent(&agent_id)
        .expect("load recovered state")
        .expect("agent state");
    assert!(persisted_after_resume.memory.pending_turn.is_none());
    assert!(persisted_after_resume.memory.run.is_none());
    assert!(persisted_after_resume.memory.transcript.is_empty());
}

#[tokio::test]
async fn resume_agent_keeps_committed_transcript_when_tool_execution_was_interrupted() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let store = temp_store("committed-recovery");
    let tool_started = Arc::new(Notify::new());
    let tool_release = Arc::new(Notify::new());
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![tool_use_stream(
            &model.id,
            "tool-1",
            "blocking_tool",
            r#"{"value":"hi"}"#,
        )],
    );

    let runtime = Runtime::empty_builder()
        .with_store(store.clone())
        .with_provider_instance(provider)
        .with_tool(BlockingTool {
            started: tool_started.clone(),
            release: tool_release,
        })
        .build()
        .expect("build runtime");
    let agent = runtime.spawn("agent", model.clone()).expect("spawn agent");
    let agent_id = agent.id().to_string();

    let send_task = tokio::spawn(async move {
        let mut agent = agent;
        let result = agent
            .send(vec![ContentBlock::Text {
                text: "hello".to_string(),
            }])
            .await;
        (agent, result)
    });

    tool_started.notified().await;
    send_task.abort();
    let _ = send_task.await;
    clear_leases(&store);

    let persisted_before_resume = store
        .load_agent(&agent_id)
        .expect("load interrupted state")
        .expect("agent state");
    assert!(persisted_before_resume.memory.pending_turn.is_none());
    assert!(
        persisted_before_resume
            .memory
            .run
            .as_ref()
            .expect("run metadata")
            .assistant_committed
    );

    let reboot_runtime = Runtime::empty_builder()
        .with_store(store.clone())
        .with_provider_instance(ScriptedProvider::new(
            BuiltinProvider::Anthropic,
            vec![model.clone()],
            Vec::new(),
        ))
        .build()
        .expect("rebuild runtime");
    let resumed = reboot_runtime
        .resume_agent(&agent_id)
        .expect("resume interrupted agent");

    assert_eq!(
        resumed.watch_snapshot().borrow().status,
        AgentStatus::Interrupted
    );
    assert_eq!(resumed.history().len(), 2);
    assert_eq!(
        resumed.history()[0],
        Message::user(ContentBlock::text("hello"))
    );
    assert!(matches!(
        &resumed.history()[1].content[0],
        ContentBlock::ToolUse { name, .. } if name == "blocking_tool"
    ));

    let persisted_after_resume = store
        .load_agent(&agent_id)
        .expect("load recovered state")
        .expect("agent state");
    assert!(persisted_after_resume.memory.run.is_none());
    assert_eq!(persisted_after_resume.memory.transcript.len(), 2);
}

#[tokio::test]
async fn resume_all_rebuilds_agents_from_agent_memory() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let store = temp_store("resume-all");
    let runtime = Runtime::empty_builder()
        .with_store(store.clone())
        .with_provider_instance(ScriptedProvider::new(
            BuiltinProvider::Anthropic,
            vec![model.clone()],
            vec![text_stream(&model.id, "done")],
        ))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model.clone()).expect("spawn agent");
    let agent_id = agent.id().to_string();

    agent
        .send(vec![ContentBlock::Text {
            text: "hello".to_string(),
        }])
        .await
        .expect("send");
    clear_leases(&store);

    let reboot_runtime = Runtime::empty_builder()
        .with_store(store.clone())
        .with_provider_instance(ScriptedProvider::new(
            BuiltinProvider::Anthropic,
            vec![model],
            Vec::new(),
        ))
        .build()
        .expect("rebuild runtime");
    let resumed = reboot_runtime.resume_all().expect("resume all");

    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0].id(), agent_id);
    assert_eq!(resumed[0].history().len(), 2);
    assert_eq!(
        resumed[0].last_message(),
        Some(&Message::assistant(ContentBlock::text("done")))
    );
}

#[tokio::test]
async fn resume_filters_agents_by_runtime_identifier() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let store = temp_store("resume-filter");

    let runtime_a = Runtime::empty_builder()
        .with_runtime_identifier("session-a")
        .with_store(store.clone())
        .with_provider_instance(ScriptedProvider::new(
            BuiltinProvider::Anthropic,
            vec![model.clone()],
            Vec::new(),
        ))
        .build()
        .expect("build runtime a");
    let agent_a = runtime_a
        .spawn("agent-a", model.clone())
        .expect("spawn agent a");

    let runtime_b = Runtime::empty_builder()
        .with_runtime_identifier("session-b")
        .with_store(store.clone())
        .with_provider_instance(ScriptedProvider::new(
            BuiltinProvider::Anthropic,
            vec![model.clone()],
            Vec::new(),
        ))
        .build()
        .expect("build runtime b");
    let _agent_b = runtime_b
        .spawn("agent-b", model.clone())
        .expect("spawn agent b");

    clear_leases(&store);

    let reboot_runtime = Runtime::empty_builder()
        .with_runtime_identifier("session-a")
        .with_store(store.clone())
        .with_provider_instance(ScriptedProvider::new(
            BuiltinProvider::Anthropic,
            vec![model],
            Vec::new(),
        ))
        .build()
        .expect("rebuild runtime");

    let resumed = reboot_runtime
        .resume("session-a")
        .expect("resume session-a");
    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0].id(), agent_a.id());
    assert_eq!(resumed[0].name(), "agent-a");
}

#[tokio::test]
async fn list_persisted_agents_includes_teammates_for_runtime() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let store = temp_store("persisted-agent-list");
    let runtime_identifier = "persisted-agent-list";

    let runtime = Runtime::builder()
        .with_runtime_identifier(runtime_identifier)
        .with_store(store.clone())
        .with_provider_instance(ScriptedProvider::new(
            BuiltinProvider::Anthropic,
            vec![model.clone()],
            Vec::new(),
        ))
        .build()
        .expect("build runtime");
    let mut lead = runtime
        .spawn_with_config(
            "lead",
            model,
            AgentConfig {
                team: team_config(temp_team_dir("persisted-agent-list-team")),
                ..Default::default()
            },
        )
        .expect("spawn lead");
    lead.spawn_teammate("alice", "researcher", None)
        .await
        .expect("spawn teammate");

    let persisted = runtime
        .list_persisted_agents(runtime_identifier)
        .expect("list persisted agents");
    assert_eq!(persisted.len(), 2);
    assert_eq!(persisted[0].name, "lead");
    assert!(!persisted[0].is_teammate);
    assert_eq!(persisted[1].name, "alice");
    assert!(persisted[1].is_teammate);
}

#[tokio::test]
async fn dropping_runtime_releases_agent_lease_for_next_resume() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let store = temp_store("lease-release");

    let runtime = Runtime::empty_builder()
        .with_runtime_identifier("lease-release")
        .with_store(store.clone())
        .with_provider_instance(ScriptedProvider::new(
            BuiltinProvider::Anthropic,
            vec![model.clone()],
            vec![text_stream(&model.id, "done")],
        ))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model.clone()).expect("spawn agent");
    let agent_id = agent.id().to_string();

    agent
        .send(vec![ContentBlock::Text {
            text: "hello".to_string(),
        }])
        .await
        .expect("send");
    drop(agent);
    drop(runtime);

    let reboot_runtime = Runtime::empty_builder()
        .with_runtime_identifier("lease-release")
        .with_store(store)
        .with_provider_instance(ScriptedProvider::new(
            BuiltinProvider::Anthropic,
            vec![model],
            Vec::new(),
        ))
        .build()
        .expect("rebuild runtime");

    let resumed = reboot_runtime
        .resume("lease-release")
        .expect("resume runtime after drop");
    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0].id(), agent_id);
}

#[tokio::test]
async fn resume_revives_persisted_teammate_actors_for_lead_runtime() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let store = temp_store("teammate-revive-resume");
    let runtime_identifier = "teammate-revive";

    let initial_runtime = Runtime::builder()
        .with_runtime_identifier(runtime_identifier)
        .with_store(store.clone())
        .with_provider_instance(ScriptedProvider::new(
            BuiltinProvider::Anthropic,
            vec![model.clone()],
            Vec::new(),
        ))
        .build()
        .expect("build initial runtime");
    let mut lead = initial_runtime
        .spawn_with_config(
            "lead",
            model.clone(),
            AgentConfig {
                team: team_config(temp_team_dir("resume-revive-team")),
                ..Default::default()
            },
        )
        .expect("spawn lead");
    lead.spawn_teammate("alice", "researcher", None)
        .await
        .expect("spawn teammate");
    drop(lead);
    drop(initial_runtime);
    clear_leases(&store);

    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "revived-send",
                "team_send",
                r#"{"to":"lead","content":"revived and responsive"}"#,
            ),
            text_stream(&model.id, "done"),
            text_stream(&model.id, "checked"),
        ],
    );
    let provider_handle = provider.clone();
    let runtime = Runtime::builder()
        .with_runtime_identifier(runtime_identifier)
        .with_store(store)
        .with_provider_instance(provider)
        .build()
        .expect("build resumed runtime");

    let mut resumed = runtime.resume(runtime_identifier).expect("resume runtime");
    assert_eq!(resumed.len(), 1);
    let mut lead = resumed.pop().expect("lead agent");
    assert!(!lead.is_teammate());

    lead.send_team_message("alice", "Ping me after restart")
        .expect("send team message");
    wait_for_recorded_requests(&provider_handle, 2).await;
    wait_for_teammate_status(&lead, TeamMemberStatus::Idle).await;

    lead.send(vec![ContentBlock::Text {
        text: "status?".to_string(),
    }])
    .await
    .expect("send status check");

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 3);
    let inbox = latest_team_inbox_text(&requests[2]).expect("team inbox");
    assert!(inbox.contains("alice"));
    assert!(inbox.contains("revived and responsive"));
}

struct BlockingTool {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl ExecutableTool for BlockingTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "blocking_tool".to_string(),
            description: Some("blocks until released".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string" }
                }
            }),
            capabilities: vec![],
            side_effect_level: ToolSideEffectLevel::None,
            durability: ToolDurability::ReplaySafe,
        }
    }

    async fn execute_mut(&self, _ctx: ToolContext<'_>, _input: Value) -> ToolResult {
        self.started.notify_one();
        self.release.notified().await;
        Ok("released".to_string())
    }
}

async fn wait_for_status(receiver: &mut watch::Receiver<AgentSnapshot>, status: AgentStatus) {
    loop {
        if receiver.borrow().status == status {
            return;
        }
        receiver.changed().await.expect("snapshot changed");
    }
}

async fn wait_for_teammate_status(agent: &crate::agent::Agent, status: TeamMemberStatus) {
    let mut receiver = agent.watch_snapshot();
    loop {
        if receiver
            .borrow()
            .teammates
            .iter()
            .any(|teammate| teammate.name == "alice" && teammate.status == status)
        {
            return;
        }
        receiver.changed().await.expect("snapshot changed");
    }
}

fn tool_use_stream(
    model: &str,
    id: &str,
    name: &str,
    input_json: &str,
) -> super::support::StreamScript {
    ok_stream(vec![
        ProviderEvent::MessageStarted {
            id: format!("msg-{id}"),
            model: model.to_string(),
            role: Role::Assistant,
        },
        ProviderEvent::ContentBlockStarted {
            index: 0,
            kind: ContentBlockStart::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
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

fn text_stream(model: &str, text: &str) -> super::support::StreamScript {
    ok_stream(vec![
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
    ])
}

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

fn temp_store(label: &str) -> SqliteRuntimeStore {
    let unique = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "mentra-runtime-resume-{label}-{timestamp}-{unique}.sqlite"
    ));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create temp dir");
    }
    SqliteRuntimeStore::new(path)
}

fn temp_team_dir(label: &str) -> std::path::PathBuf {
    let unique = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let path =
        std::env::temp_dir().join(format!("mentra-runtime-team-{label}-{timestamp}-{unique}"));
    fs::create_dir_all(&path).expect("create temp team dir");
    path
}

fn team_config(team_dir: std::path::PathBuf) -> TeamConfig {
    TeamConfig {
        team_dir,
        ..Default::default()
    }
}

fn clear_leases(store: &SqliteRuntimeStore) {
    let conn = rusqlite::Connection::open(store.path()).expect("open store");
    conn.execute("DELETE FROM leases", [])
        .expect("clear leases");
}

async fn wait_for_recorded_requests(provider: &ScriptedProvider, expected: usize) {
    for _ in 0..50 {
        if provider.recorded_requests().await.len() >= expected {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {expected} recorded requests");
}

fn latest_team_inbox_text(request: &crate::provider::Request<'_>) -> Option<String> {
    request.messages.iter().rev().find_map(|message| {
        message.content.iter().find_map(|block| match block {
            ContentBlock::Text { text } if text.contains("<team-inbox>") => Some(text.clone()),
            _ => None,
        })
    })
}
