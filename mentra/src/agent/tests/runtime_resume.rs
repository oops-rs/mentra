use std::{
    collections::BTreeMap,
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
    BuiltinProvider, ContentBlock, Message, ProviderId, ReasoningFormat, ReasoningProvenance, Role,
    TranscriptKind,
    agent::{AgentConfig, AgentSnapshot, AgentStatus, TeamConfig},
    provider::{ContentBlockDelta, ContentBlockStart, ProviderEvent},
    runtime::{AgentStore, Runtime, SqliteRuntimeStore},
    team::{TeamMemberStatus, TeamMessage, TeamStore},
    tool::{
        ToolContext, ToolDefinition, ToolDurability, ToolExecutor, ToolOutput, ToolResult,
        ToolSideEffectLevel, ToolSpec,
    },
};

use super::support::{ScriptedProvider, controlled_stream, erroring_stream, model_info, ok_stream};

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
async fn aborted_partial_thinking_stream_rolls_back_instead_of_persisting_empty_signature() {
    let provider_id = ProviderId::new("anthropic-edge");
    let model = model_info("claude-requested", provider_id.clone());
    let store = temp_store("aborted-thinking");
    let provider = ScriptedProvider::new(
        provider_id.clone(),
        vec![model.clone()],
        vec![erroring_stream(
            vec![
                ProviderEvent::MessageStarted {
                    id: "msg-thinking".to_string(),
                    model: model.id.clone(),
                    role: Role::Assistant,
                },
                ProviderEvent::ContentBlockStarted {
                    index: 0,
                    kind: ContentBlockStart::Thinking {
                        encrypted_content: None,
                        id: None,
                        provenance: Some(ReasoningProvenance {
                            provider: provider_id,
                            model: model.id.clone(),
                            format: ReasoningFormat::AnthropicSigned,
                        }),
                        redacted: false,
                    },
                },
                ProviderEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::ThinkingText("partial chain".to_string()),
                },
                ProviderEvent::ContentBlockStopped { index: 0 },
            ],
            crate::ProviderError::MalformedStream("aborted".to_string()),
        )],
    );
    let runtime = Runtime::empty_builder()
        .with_store(store.clone())
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");
    let agent_id = agent.id().to_string();

    agent
        .send(vec![ContentBlock::text("hello")])
        .await
        .expect_err("aborted stream should fail");

    assert!(agent.history().is_empty());
    let persisted = store
        .load_agent(&agent_id)
        .expect("load agent")
        .expect("persisted agent");
    assert!(persisted.memory.transcript.is_empty());
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
async fn signed_and_redacted_thinking_survive_commit_persist_resume_and_replay() {
    let provider_id = ProviderId::new("anthropic-edge");
    let model = model_info("claude-requested", provider_id.clone());
    let store = temp_store("thinking-replay");
    let expected_thinking = vec![
        ContentBlock::Thinking {
            thinking: "private chain".to_string(),
            signature: Some("opaque-signature".to_string()),
            encrypted_content: None,
            id: None,
            provenance: Some(ReasoningProvenance {
                provider: provider_id.clone(),
                model: model.id.clone(),
                format: ReasoningFormat::AnthropicSigned,
            }),
            redacted: false,
        },
        ContentBlock::Thinking {
            thinking: String::new(),
            signature: Some("opaque-redacted-data".to_string()),
            encrypted_content: None,
            id: None,
            provenance: Some(ReasoningProvenance {
                provider: provider_id.clone(),
                model: model.id.clone(),
                format: ReasoningFormat::AnthropicSigned,
            }),
            redacted: true,
        },
    ];
    let first_provider = ScriptedProvider::new(
        provider_id.clone(),
        vec![model.clone()],
        vec![thinking_stream(
            &provider_id,
            &model.id,
            "private chain",
            "opaque-signature",
            "opaque-redacted-data",
            "visible answer",
        )],
    );
    let runtime = Runtime::empty_builder()
        .with_store(store.clone())
        .with_provider_instance(first_provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model.clone()).expect("spawn agent");
    let agent_id = agent.id().to_string();

    let response = agent
        .send(vec![ContentBlock::text("hello")])
        .await
        .expect("commit thinking response");
    assert_eq!(&response.content[..2], expected_thinking.as_slice());
    assert_eq!(response.text(), "visible answer");

    let persisted = store
        .load_agent(&agent_id)
        .expect("load agent")
        .expect("persisted agent");
    let persisted_assistant = persisted
        .memory
        .transcript
        .to_messages()
        .into_iter()
        .find(|message| message.role == Role::Assistant)
        .expect("persisted assistant message");
    assert_eq!(
        &persisted_assistant.content[..2],
        expected_thinking.as_slice()
    );
    clear_leases(&store);

    let replay_provider = ScriptedProvider::new(
        provider_id,
        vec![model.clone()],
        vec![text_stream(&model.id, "continued")],
    );
    let reboot_runtime = Runtime::empty_builder()
        .with_store(store)
        .with_provider_instance(replay_provider.clone())
        .build()
        .expect("rebuild runtime");
    let mut resumed = reboot_runtime
        .resume_agent(&agent_id)
        .expect("resume persisted agent");

    resumed
        .send(vec![ContentBlock::text("continue")])
        .await
        .expect("replay persisted thinking");
    let requests = replay_provider.recorded_requests().await;
    let replayed_assistant = requests[0]
        .messages
        .iter()
        .find(|message| message.role == Role::Assistant)
        .expect("replayed assistant message");
    assert_eq!(
        &replayed_assistant.content[..2],
        expected_thinking.as_slice()
    );
}

#[tokio::test]
async fn responses_reasoning_and_paired_tool_ids_survive_agent_replay() {
    let provider_id = ProviderId::new("openai-edge");
    let model = model_info("gpt-requested", provider_id.clone());
    let provider = ScriptedProvider::new(
        provider_id.clone(),
        vec![model.clone()],
        vec![
            responses_reasoning_tool_stream(&provider_id, &model.id),
            text_stream(&model.id, "done"),
        ],
    );
    let runtime = Runtime::empty_builder()
        .with_store(temp_store("responses-reasoning-replay"))
        .with_provider_instance(provider.clone())
        .with_tool(DetailsTool)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    agent
        .send(vec![ContentBlock::text("use the tool")])
        .await
        .expect("Responses reasoning tool loop should complete");

    let requests = provider.recorded_requests().await;
    assert_eq!(requests.len(), 2);
    let replayed_assistant = requests[1]
        .messages
        .iter()
        .find(|message| message.role == Role::Assistant)
        .expect("assistant reasoning turn should replay");
    assert!(matches!(
        &replayed_assistant.content[0],
        ContentBlock::Thinking {
            id: Some(id),
            encrypted_content: Some(encrypted_content),
            ..
        } if id == "rs_1" && encrypted_content == "encrypted-1"
    ));
    assert!(matches!(
        &replayed_assistant.content[1],
        ContentBlock::ToolUse { id, .. } if id == "call_1|fc_1"
    ));
    assert!(requests[1].messages.iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(
                block,
                ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "call_1|fc_1"
            )
        })
    }));
}

// M3 test 1: `ToolOutput::details` survives a real restart — persisted to
// the SQLite `agent_memory` row by a live tool round, then recovered by a
// brand new `AgentMemory` built from `RuntimeStore::load_agent` (the same
// `resume_all` path every crash-recovery test in this file exercises),
// keyed by the originating `tool_use_id`.
#[tokio::test]
async fn resumed_agent_keeps_tool_result_details_after_restart() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let store = temp_store("resume-details");
    let runtime = Runtime::empty_builder()
        .with_store(store.clone())
        .with_provider_instance(ScriptedProvider::new(
            BuiltinProvider::Anthropic,
            vec![model.clone()],
            vec![
                tool_use_stream(&model.id, "call-1", "details_tool", r#"{}"#),
                text_stream(&model.id, "done"),
            ],
        ))
        .with_tool(DetailsTool)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model.clone()).expect("spawn agent");
    let agent_id = agent.id().to_string();

    agent
        .send(vec![ContentBlock::Text {
            text: "run the details tool".to_string(),
        }])
        .await
        .expect("send");
    clear_leases(&store);

    let reboot_runtime = Runtime::empty_builder()
        .with_store(store)
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

    let item = resumed[0]
        .transcript()
        .items()
        .iter()
        .find(|item| matches!(item.kind, TranscriptKind::ToolExchange { .. }))
        .expect("resumed transcript keeps the tool exchange item");
    assert_eq!(
        item.details(),
        Some(&BTreeMap::from([(
            "call-1".to_string(),
            json!({ "marker": "keep-me" }),
        )]))
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

#[tokio::test]
async fn resume_wakes_revived_teammate_for_persisted_inbox_work() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let store = temp_store("teammate-revive-pending-inbox");
    let runtime_identifier = "teammate-revive-pending-inbox";
    let team_dir = temp_team_dir("resume-pending-inbox-team");

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
                team: team_config(team_dir.clone()),
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

    <SqliteRuntimeStore as TeamStore>::append_team_message(
        &store,
        team_dir.as_path(),
        "alice",
        &TeamMessage::message(
            "lead".to_string(),
            "Handle this persisted inbox work after restart".to_string(),
        ),
    )
    .expect("append persisted inbox message");

    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "persisted-inbox-send",
                "team_send",
                r#"{"to":"lead","content":"processed persisted inbox"}"#,
            ),
            text_stream(&model.id, "done"),
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
    let lead = resumed.pop().expect("lead agent");
    assert!(!lead.is_teammate());

    wait_for_recorded_requests(&provider_handle, 2).await;
    wait_for_teammate_status(&lead, TeamMemberStatus::Idle).await;

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 2);
    let inbox = latest_team_inbox_text(&requests[0]).expect("team inbox");
    assert!(inbox.contains("Handle this persisted inbox work after restart"));
}

/// A tool that returns opaque `ToolOutput::details` metadata, used to prove
/// details survive a real restart (persist/reload through the SQLite store).
struct DetailsTool;

#[async_trait]
impl ToolDefinition for DetailsTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("details_tool")
            .description("test tool: returns opaque details metadata")
            .input_schema(json!({
                "type": "object",
                "properties": {}
            }))
            .side_effect_level(ToolSideEffectLevel::None)
            .durability(ToolDurability::ReplaySafe)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for DetailsTool {
    async fn execute_mut_output(
        &self,
        _ctx: ToolContext<'_>,
        _input: Value,
    ) -> Result<ToolOutput, String> {
        Ok(ToolOutput::text("tool output").with_details(json!({ "marker": "keep-me" })))
    }
}

struct BlockingTool {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl ToolDefinition for BlockingTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("blocking_tool")
            .description("blocks until released")
            .input_schema(json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string" }
                }
            }))
            .side_effect_level(ToolSideEffectLevel::None)
            .durability(ToolDurability::ReplaySafe)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for BlockingTool {
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

fn thinking_stream(
    provider: &ProviderId,
    model: &str,
    thinking: &str,
    signature: &str,
    redacted_data: &str,
    text: &str,
) -> super::support::StreamScript {
    let provenance = Some(ReasoningProvenance {
        provider: provider.clone(),
        model: model.to_string(),
        format: ReasoningFormat::AnthropicSigned,
    });
    ok_stream(vec![
        ProviderEvent::MessageStarted {
            id: "msg-thinking".to_string(),
            model: model.to_string(),
            role: Role::Assistant,
        },
        ProviderEvent::ContentBlockStarted {
            index: 0,
            kind: ContentBlockStart::Thinking {
                encrypted_content: None,
                id: None,
                provenance: provenance.clone(),
                redacted: false,
            },
        },
        ProviderEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::ThinkingText(thinking.to_string()),
        },
        ProviderEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::ThinkingSignature(signature.to_string()),
        },
        ProviderEvent::ContentBlockStopped { index: 0 },
        ProviderEvent::ContentBlockStarted {
            index: 1,
            kind: ContentBlockStart::Thinking {
                encrypted_content: None,
                id: None,
                provenance,
                redacted: true,
            },
        },
        ProviderEvent::ContentBlockDelta {
            index: 1,
            delta: ContentBlockDelta::ThinkingSignature(redacted_data.to_string()),
        },
        ProviderEvent::ContentBlockStopped { index: 1 },
        ProviderEvent::ContentBlockStarted {
            index: 2,
            kind: ContentBlockStart::Text,
        },
        ProviderEvent::ContentBlockDelta {
            index: 2,
            delta: ContentBlockDelta::Text(text.to_string()),
        },
        ProviderEvent::ContentBlockStopped { index: 2 },
        ProviderEvent::MessageStopped,
    ])
}

fn responses_reasoning_tool_stream(
    provider: &ProviderId,
    model: &str,
) -> super::support::StreamScript {
    ok_stream(vec![
        ProviderEvent::MessageStarted {
            id: "resp-reasoning-tool".to_string(),
            model: model.to_string(),
            role: Role::Assistant,
        },
        ProviderEvent::ContentBlockStarted {
            index: 0,
            kind: ContentBlockStart::Thinking {
                encrypted_content: None,
                id: Some("rs_1".to_string()),
                provenance: Some(ReasoningProvenance {
                    provider: provider.clone(),
                    model: model.to_string(),
                    format: ReasoningFormat::OpenAiEncrypted,
                }),
                redacted: false,
            },
        },
        ProviderEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::ThinkingText("short summary".to_string()),
        },
        ProviderEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::ThinkingEncryptedContent("encrypted-1".to_string()),
        },
        ProviderEvent::ContentBlockStopped { index: 0 },
        ProviderEvent::ContentBlockStarted {
            index: 1,
            kind: ContentBlockStart::ToolUse {
                id: "call_1|fc_1".to_string(),
                name: "details_tool".to_string(),
            },
        },
        ProviderEvent::ContentBlockDelta {
            index: 1,
            delta: ContentBlockDelta::ToolUseInputJson("{}".to_string()),
        },
        ProviderEvent::ContentBlockStopped { index: 1 },
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
    for _ in 0..250 {
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
