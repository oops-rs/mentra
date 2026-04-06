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
        CompactionInputItem, CompactionResponse, ContentBlockDelta, ContentBlockStart,
        ProviderCapabilities, ProviderEvent, Request,
    },
    runtime::Runtime,
};

use crate::provider::ProviderError;

use super::support::{ScriptedProvider, StaticTool, StreamScript, erroring_stream, model_info, ok_stream};

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

fn text_stream(model: &str, text: &str) -> StreamScript {
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

fn tool_use_stream(model: &str, id: &str, name: &str, input_json: &str) -> StreamScript {
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
