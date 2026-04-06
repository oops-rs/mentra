use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    BuiltinProvider, ContentBlock, Message, Role,
    agent::{AgentConfig, AgentEvent, CompactionConfig, CompactionTrigger},
    compaction::{CompactionExecutionMode, CompactionMode},
    provider::{
        CompactionInputItem, CompactionResponse, ProviderCapabilities, Request,
    },
    runtime::{Runtime, SqliteRuntimeStore},
};

use crate::provider::ProviderError;

use super::support::{
    ScriptedProvider, SessionGenerator, StaticTool, erroring_stream, model_info, text_stream,
    tool_use_stream,
};

#[tokio::test]
async fn micro_compaction_only_rewrites_old_tool_results_in_requests() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let long_output = "x".repeat(140);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "tool-1", "echo_tool", r#"{"value":"one"}"#),
            tool_use_stream(&model.id, "tool-2", "echo_tool", r#"{"value":"two"}"#),
            tool_use_stream(&model.id, "tool-3", "echo_tool", r#"{"value":"three"}"#),
            tool_use_stream(&model.id, "tool-4", "echo_tool", r#"{"value":"four"}"#),
            text_stream(&model.id, "done"),
        ],
    );
    let provider_handle = provider.clone();

    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("echo_tool", &long_output))
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                compaction: CompactionConfig {
                    keep_recent_tool_results: 2,
                    auto_compact_threshold_tokens: None,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();

    agent
        .send(vec![ContentBlock::Text {
            text: "hello".to_string(),
        }])
        .await
        .unwrap();

    assert_eq!(
        agent.history()[2],
        Message::user(ContentBlock::ToolResult {
            tool_use_id: "tool-1".to_string(),
            content: long_output.clone().into(),
            is_error: false,
        })
    );

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 5);
    let final_tool_results = tool_result_contents(&requests[4]);
    assert_eq!(
        final_tool_results,
        vec![
            "[Previous: used echo_tool]".to_string(),
            "[Previous: used echo_tool]".to_string(),
            long_output.clone(),
            long_output,
        ]
    );
}

#[tokio::test]
async fn auto_compaction_persists_transcript_and_rewrites_history() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            text_stream(&model.id, "first done"),
            text_stream(&model.id, "summary"),
            text_stream(&model.id, "second done"),
        ],
    );
    let provider_handle = provider.clone();
    let transcript_dir = temp_dir("auto-compact");

    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                compaction: CompactionConfig {
                    auto_compact_threshold_tokens: Some(1),
                    transcript_dir: transcript_dir.clone(),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = agent.subscribe_events();

    agent
        .send(vec![ContentBlock::Text {
            text: "first".to_string(),
        }])
        .await
        .unwrap();
    agent
        .send(vec![ContentBlock::Text {
            text: "second".to_string(),
        }])
        .await
        .unwrap();

    assert_eq!(agent.history().len(), 4);
    assert_eq!(agent.history()[0].role, Role::User);
    assert_eq!(message_text(&agent.history()[0]), "first");
    assert!(message_text(&agent.history()[1]).contains("[Compaction summary]"));
    assert!(message_text(&agent.history()[1]).contains("Progress: summary"));

    let transcripts = fs::read_dir(&transcript_dir)
        .expect("read transcript dir")
        .map(|entry| entry.expect("read transcript entry").path())
        .collect::<Vec<_>>();
    assert_eq!(transcripts.len(), 1);

    let transcript = fs::read_to_string(&transcripts[0]).expect("read transcript");
    assert_eq!(transcript.lines().count(), 3);

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 3);
    assert!(requests[1].tools.is_empty());
    assert_eq!(requests[1].tool_choice, None);
    assert_eq!(message_text(&requests[2].messages[0]), "first");
    assert!(
        requests[2]
            .messages
            .iter()
            .any(|message| message_text(message).contains("Progress: summary"))
    );

    let compaction = collect_events(&mut events)
        .into_iter()
        .find_map(|event| match event {
            AgentEvent::ContextCompacted { details } => Some(details),
            _ => None,
        })
        .expect("expected compaction event");
    assert_eq!(compaction.trigger, CompactionTrigger::Auto);
    assert_eq!(compaction.replaced_items, 2);
    assert_eq!(compaction.preserved_items, 1);
    assert_eq!(compaction.preserved_user_turns, 1);
    assert_eq!(compaction.preserved_delegation_results, 0);
    assert_eq!(compaction.resulting_transcript_len, 3);
    assert!(compaction.transcript_path.starts_with(&transcript_dir));
}

#[tokio::test]
async fn compact_tool_compacts_history_and_continues() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "compact-1", "compact", "{}"),
            text_stream(&model.id, "summary"),
            text_stream(&model.id, "after compact"),
        ],
    );
    let provider_handle = provider.clone();
    let transcript_dir = temp_dir("manual-compact");

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                compaction: CompactionConfig {
                    auto_compact_threshold_tokens: None,
                    transcript_dir,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = agent.subscribe_events();

    agent
        .send(vec![ContentBlock::Text {
            text: "please compact".to_string(),
        }])
        .await
        .unwrap();

    assert_eq!(agent.history().len(), 5);
    assert_eq!(message_text(&agent.history()[0]), "please compact");
    assert!(message_text(&agent.history()[1]).contains("[Compaction summary]"));
    assert!(message_text(&agent.history()[1]).contains("Progress: summary"));
    assert!(matches!(
        &agent.history()[3].content[0],
        ContentBlock::ToolResult { is_error: false, content, .. }
            if content.starts_with("Context compacted. Transcript saved to ")
    ));

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 3);
    assert!(requests[1].tools.is_empty());
    assert_eq!(requests[1].tool_choice, None);
    assert_eq!(message_text(&requests[2].messages[0]), "please compact");
    assert!(
        requests[2]
            .messages
            .iter()
            .any(|message| message_text(message).contains("Progress: summary"))
    );
    assert!(tool_names(&requests[0]).contains("compact"));

    let compaction = collect_events(&mut events)
        .into_iter()
        .find_map(|event| match event {
            AgentEvent::ContextCompacted { details } => Some(details),
            _ => None,
        })
        .expect("expected compaction event");
    assert_eq!(compaction.trigger, CompactionTrigger::Manual);
    assert_eq!(compaction.replaced_items, 1);
    assert_eq!(compaction.preserved_items, 1);
    assert_eq!(compaction.preserved_user_turns, 1);
    assert_eq!(compaction.preserved_delegation_results, 0);
    assert_eq!(compaction.resulting_transcript_len, 3);
}

#[tokio::test]
async fn auto_compaction_degrades_gracefully_on_failure() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    // Queue: first send response, then 3 retryable errors for compaction attempts,
    // then the second send response. The compaction will fail all 3 attempts and
    // degrade gracefully, allowing the second send to succeed.
    let retryable_error = || {
        erroring_stream(
            vec![],
            ProviderError::Retryable {
                message: "rate limited".into(),
                delay: None,
            },
        )
    };
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            text_stream(&model.id, "first done"),
            retryable_error(),
            retryable_error(),
            retryable_error(),
            text_stream(&model.id, "second done"),
        ],
    );
    let events_receiver = {
        let runtime = Runtime::empty_builder()
            .with_provider_instance(provider)
            .build()
            .expect("build runtime");
        let mut agent = runtime
            .spawn_with_config(
                "agent",
                model,
                AgentConfig {
                    compaction: CompactionConfig {
                        auto_compact_threshold_tokens: Some(1),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .unwrap();
        let mut events = agent.subscribe_events();

        agent
            .send(vec![ContentBlock::Text {
                text: "first".to_string(),
            }])
            .await
            .unwrap();

        // Second send triggers auto_compact_if_needed which fails all 3 attempts,
        // then degrades gracefully, and the actual send succeeds.
        agent
            .send(vec![ContentBlock::Text {
                text: "second".to_string(),
            }])
            .await
            .expect("second send must succeed despite compaction failures");

        // History should have all 4 turns (no compaction was applied).
        assert_eq!(agent.history().len(), 4, "history should have 4 items");

        collect_events(&mut events)
    };

    // Should have seen 2 RetryAttempt events (attempts 1 and 2; attempt 3 exhausts
    // without emitting because there is no further retry after the last attempt).
    let retry_events: Vec<_> = events_receiver
        .iter()
        .filter(|e| matches!(e, AgentEvent::RetryAttempt { .. }))
        .collect();
    assert_eq!(
        retry_events.len(),
        2,
        "expected 2 retry attempt events, got {}",
        retry_events.len()
    );

    // No ContextCompacted event should have been emitted.
    let compacted = events_receiver
        .iter()
        .any(|e| matches!(e, AgentEvent::ContextCompacted { .. }));
    assert!(!compacted, "expected no ContextCompacted event");
}

#[tokio::test]
async fn remote_compaction_succeeds_when_provider_supports_it() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            text_stream(&model.id, "first done"),
            text_stream(&model.id, "second done"),
        ],
    )
    .with_capabilities(ProviderCapabilities {
        supports_history_compaction: true,
        ..Default::default()
    });

    provider
        .push_compact_response(Ok(CompactionResponse {
            output: vec![CompactionInputItem::CompactionSummary {
                content: "Summary of previous work".to_string(),
            }],
        }))
        .await;

    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                compaction: CompactionConfig {
                    auto_compact_threshold_tokens: Some(1),
                    mode: CompactionMode::PreferRemote,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = agent.subscribe_events();

    agent
        .send(vec![ContentBlock::Text {
            text: "first".to_string(),
        }])
        .await
        .unwrap();
    agent
        .send(vec![ContentBlock::Text {
            text: "second".to_string(),
        }])
        .await
        .unwrap();

    let compaction = collect_events(&mut events)
        .into_iter()
        .find_map(|event| match event {
            AgentEvent::ContextCompacted { details } => Some(details),
            _ => None,
        })
        .expect("expected compaction event");
    assert_eq!(compaction.mode, CompactionExecutionMode::Remote);
}

#[tokio::test]
async fn remote_compaction_falls_back_to_local_on_unsupported() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    // Provider advertises remote support but compact() returns UnsupportedCapability
    // (no compact scripts pushed — default error).
    // Local summarization calls provider.stream(), so we need an extra text stream for it.
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            text_stream(&model.id, "first done"),
            text_stream(&model.id, "summary"),
            text_stream(&model.id, "second done"),
        ],
    )
    .with_capabilities(ProviderCapabilities {
        supports_history_compaction: true,
        ..Default::default()
    });

    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                compaction: CompactionConfig {
                    auto_compact_threshold_tokens: Some(1),
                    mode: CompactionMode::PreferRemote,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = agent.subscribe_events();

    agent
        .send(vec![ContentBlock::Text {
            text: "first".to_string(),
        }])
        .await
        .unwrap();
    agent
        .send(vec![ContentBlock::Text {
            text: "second".to_string(),
        }])
        .await
        .unwrap();

    let compaction = collect_events(&mut events)
        .into_iter()
        .find_map(|event| match event {
            AgentEvent::ContextCompacted { details } => Some(details),
            _ => None,
        })
        .expect("expected compaction event");
    assert_eq!(compaction.mode, CompactionExecutionMode::Local);
}

#[tokio::test]
async fn remote_compaction_falls_back_to_local_on_empty_remote_response() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    // Provider advertises remote support but returns an empty response — compact_remotely
    // returns Ok(None) which triggers a local fallback.
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            text_stream(&model.id, "first done"),
            text_stream(&model.id, "summary"),
            text_stream(&model.id, "second done"),
        ],
    )
    .with_capabilities(ProviderCapabilities {
        supports_history_compaction: true,
        ..Default::default()
    });

    provider
        .push_compact_response(Ok(CompactionResponse { output: vec![] }))
        .await;

    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                compaction: CompactionConfig {
                    auto_compact_threshold_tokens: Some(1),
                    mode: CompactionMode::PreferRemote,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = agent.subscribe_events();

    agent
        .send(vec![ContentBlock::Text {
            text: "first".to_string(),
        }])
        .await
        .unwrap();
    agent
        .send(vec![ContentBlock::Text {
            text: "second".to_string(),
        }])
        .await
        .unwrap();

    let compaction = collect_events(&mut events)
        .into_iter()
        .find_map(|event| match event {
            AgentEvent::ContextCompacted { details } => Some(details),
            _ => None,
        })
        .expect("expected compaction event");
    assert_eq!(compaction.mode, CompactionExecutionMode::Local);
}

// ---------------------------------------------------------------------------
// Moderate CI integration tests — multi-turn sessions with compaction cycles
// ---------------------------------------------------------------------------

/// Runs 50 turns with a low auto-compact threshold to trigger multiple compaction
/// cycles, then asserts that compaction fired at least twice and that history was
/// meaningfully reduced.
#[tokio::test]
async fn fifty_turn_session_with_multiple_compaction_cycles() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let transcript_dir = temp_dir("fifty-turn-multi-compact");

    // Generate 300 scripted responses for 50 actual sends.
    // With a very low threshold every turn can trigger compaction, and each
    // compaction consumes one extra response for the local summarizer.
    // 300 gives generous headroom even if compaction fires on every turn.
    let scripts = SessionGenerator::new(&model.id)
        .with_response_size(500)
        .add_text_turns(300)
        .build();

    let provider = ScriptedProvider::new(BuiltinProvider::Anthropic, vec![model.clone()], scripts);

    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");

    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                compaction: CompactionConfig {
                    // threshold=1 guarantees compaction fires before every turn
                    // after the first response is committed to history.
                    auto_compact_threshold_tokens: Some(1),
                    transcript_dir,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();

    let mut events = agent.subscribe_events();
    let mut compaction_count = 0usize;

    for i in 0..50u32 {
        agent
            .send(vec![ContentBlock::Text {
                text: format!("Turn {i}"),
            }])
            .await
            .unwrap_or_else(|e| panic!("turn {i} failed: {e}"));
        // Drain the event channel after each turn to avoid broadcast overflow.
        compaction_count += collect_events(&mut events)
            .iter()
            .filter(|e| matches!(e, AgentEvent::ContextCompacted { .. }))
            .count();
    }

    assert!(
        compaction_count >= 2,
        "expected at least 2 compaction cycles after 50 turns, got {compaction_count}"
    );

    // History should be compressed — without compaction it would be 100 messages
    // (50 user + 50 assistant). With compaction each cycle replaces most history
    // with a single summary message.
    assert!(
        agent.history().len() < 100,
        "expected history to be compacted (< 100 messages), got {}",
        agent.history().len()
    );
}

/// Tests that a session survives persist → drop → rebuild → resume across a
/// compaction boundary. The resumed agent should be able to continue sending turns.
#[tokio::test]
async fn resumed_session_continues_after_compaction() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let transcript_dir = temp_dir("resume-after-compact");

    // Use a persistent store so we can reopen it after dropping the runtime.
    let store = temp_sqlite_store("resume-after-compact");

    // Phase 1 — run 15 turns with a low threshold to ensure at least one
    // compaction fires, then drop agent + runtime to persist state.
    {
        // With threshold=1, compaction fires on every turn after the first.
        // 15 turns need 15 turn responses + 14 summarizer responses = 29 total.
        // Generate 50 to give generous headroom.
        let scripts = SessionGenerator::new(&model.id)
            .with_response_size(500)
            .add_text_turns(50)
            .build();

        let provider =
            ScriptedProvider::new(BuiltinProvider::Anthropic, vec![model.clone()], scripts);

        let runtime = Runtime::empty_builder()
            .with_store(store.clone())
            .with_provider_instance(provider)
            .build()
            .expect("build runtime");

        let mut agent = runtime
            .spawn_with_config(
                "agent",
                model.clone(),
                AgentConfig {
                    compaction: CompactionConfig {
                        auto_compact_threshold_tokens: Some(1),
                        transcript_dir,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .unwrap();

        let mut events = agent.subscribe_events();
        let mut compaction_count = 0usize;

        for i in 0..15u32 {
            agent
                .send(vec![ContentBlock::Text {
                    text: format!("Phase-1 turn {i}"),
                }])
                .await
                .unwrap_or_else(|e| panic!("phase-1 turn {i} failed: {e}"));
            // Drain the event channel after each turn to avoid broadcast overflow.
            compaction_count += collect_events(&mut events)
                .iter()
                .filter(|e| matches!(e, AgentEvent::ContextCompacted { .. }))
                .count();
        }
        assert!(
            compaction_count >= 1,
            "expected at least 1 compaction in phase 1, got {compaction_count}"
        );

        // Dropping agent then runtime persists state and releases the lease.
        drop(agent);
        drop(runtime);
    }

    // Clear leases so the second runtime can acquire the agent.
    clear_sqlite_leases(&store);

    // Phase 2 — rebuild runtime with the same store and resume the agent.
    {
        let scripts = SessionGenerator::new(&model.id)
            .with_response_size(200)
            .add_text_turns(10)
            .build();

        let provider =
            ScriptedProvider::new(BuiltinProvider::Anthropic, vec![model.clone()], scripts);

        let new_runtime = Runtime::empty_builder()
            .with_store(store)
            .with_provider_instance(provider)
            .build()
            .expect("rebuild runtime");

        let resumed_agents = new_runtime.resume_all().expect("resume_all");
        assert_eq!(resumed_agents.len(), 1, "expected exactly one resumed agent");
        let mut agent = resumed_agents.into_iter().next().unwrap();

        for i in 0..5u32 {
            agent
                .send(vec![ContentBlock::Text {
                    text: format!("Phase-2 turn {i}"),
                }])
                .await
                .unwrap_or_else(|e| panic!("phase-2 turn {i} failed: {e}"));
        }

        // Resumed agent should have produced at least the 5 post-resume replies.
        assert!(
            !agent.history().is_empty(),
            "resumed agent should have history after additional sends"
        );
    }
}

/// Smoke test: multiple compaction cycles must not panic or corrupt the session.
/// Verifies that history survives three or more compaction cycles over 30 turns.
#[tokio::test]
async fn compaction_chain_preserves_context_across_cycles() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let transcript_dir = temp_dir("compact-chain");

    // With threshold=1, compaction fires on every turn after the first.
    // 30 turns require 30 turn responses + 29 summarizer responses = 59 total.
    // Generate 100 to give generous headroom.
    let scripts = SessionGenerator::new(&model.id)
        .with_response_size(500)
        .add_text_turns(100)
        .build();

    let provider = ScriptedProvider::new(BuiltinProvider::Anthropic, vec![model.clone()], scripts);

    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");

    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                compaction: CompactionConfig {
                    auto_compact_threshold_tokens: Some(1),
                    transcript_dir,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();

    let mut events = agent.subscribe_events();
    let mut compaction_count = 0usize;

    for i in 0..30u32 {
        agent
            .send(vec![ContentBlock::Text {
                text: format!("Turn {i}"),
            }])
            .await
            .unwrap_or_else(|e| panic!("turn {i} failed: {e}"));
        // Drain the event channel after each turn to avoid broadcast overflow.
        compaction_count += collect_events(&mut events)
            .iter()
            .filter(|e| matches!(e, AgentEvent::ContextCompacted { .. }))
            .count();
    }

    assert!(
        compaction_count >= 2,
        "expected at least 2 compaction cycles after 30 turns, got {compaction_count}"
    );

    // After multiple compaction cycles the session must still be usable —
    // history is non-empty and we didn't panic.
    assert!(
        !agent.history().is_empty(),
        "history must not be empty after compaction chain"
    );
}

fn temp_sqlite_store(label: &str) -> SqliteRuntimeStore {
    let unique = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "mentra-runtime-compact-{label}-{timestamp}-{unique}.sqlite"
    ));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create temp dir");
    }
    SqliteRuntimeStore::new(path)
}

fn clear_sqlite_leases(store: &SqliteRuntimeStore) {
    let conn = rusqlite::Connection::open(store.path()).expect("open store");
    conn.execute("DELETE FROM leases", [])
        .expect("clear leases");
}

fn tool_result_contents(request: &Request<'_>) -> Vec<String> {
    request
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.to_display_string()),
            _ => None,
        })
        .collect()
}

fn tool_names(request: &Request<'_>) -> std::collections::HashSet<String> {
    request.tools.iter().map(|tool| tool.name.clone()).collect()
}

fn message_text(message: &Message) -> &str {
    message
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or("")
}

fn collect_events(receiver: &mut tokio::sync::broadcast::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        events.push(event);
    }
    events
}

#[tokio::test]
async fn transcript_cleanup_prunes_old_files() {
    use crate::compaction::cleanup_old_transcripts;

    let dir = temp_dir("cleanup-prune");

    // Write 5 fake .jsonl files with increasing timestamps so sort order is deterministic.
    let mut filenames = Vec::new();
    for i in 0..5u64 {
        let name = format!("{:020}.jsonl", i);
        let path = dir.join(&name);
        fs::write(&path, b"{}").expect("write fake transcript");
        filenames.push(name);
    }

    // Keep only 3 (the 2 oldest should be removed).
    cleanup_old_transcripts(&dir, 3)
        .await
        .expect("cleanup should succeed");

    let remaining: std::collections::BTreeSet<String> = fs::read_dir(&dir)
        .expect("read dir")
        .map(|e| {
            e.expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();

    assert_eq!(remaining.len(), 3, "expected 3 files, got {remaining:?}");
    // The 3 newest files (indices 2, 3, 4) must survive.
    for i in 2..5u64 {
        let expected = format!("{:020}.jsonl", i);
        assert!(
            remaining.contains(&expected),
            "expected {expected} to remain, got {remaining:?}"
        );
    }
    // The 2 oldest files (indices 0, 1) must be gone.
    for i in 0..2u64 {
        let expected = format!("{:020}.jsonl", i);
        assert!(
            !remaining.contains(&expected),
            "expected {expected} to be deleted, got {remaining:?}"
        );
    }
}

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

fn temp_dir(label: &str) -> PathBuf {
    let unique = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "mentra-runtime-compact-{label}-{timestamp}-{unique}"
    ));
    fs::create_dir_all(&path).expect("create temp dir");
    path
}
