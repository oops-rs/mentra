use std::{
    fs,
    path::PathBuf,
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{
    BuiltinProvider, ContentBlock, Message, Role, SessionEvent, TokenUsage, TranscriptKind,
    agent::{
        AgentConfig, AgentEvent, CompactionConfig, CompactionTrigger, ElidedToolResult,
        ProjectedToolResultBudget, RequestToolResultElision, RequestToolResultElisionPolicy,
        ToolResultContentKind, ToolResultElisionAction,
    },
    compaction::{CompactionExecutionMode, CompactionMode},
    error::RuntimeError,
    provider::{CompactionInputItem, CompactionResponse, ProviderCapabilities, Request},
    runtime::{ProviderRetry, RunOptions, Runtime},
    tool::{
        ToolContext, ToolDefinition, ToolDurability, ToolExecutor, ToolOutput, ToolSideEffectLevel,
        ToolSpec,
    },
};

use crate::provider::ProviderError;

use super::support::{
    PersistentStore, ScriptedProvider, SessionGenerator, StaticTool, erroring_stream, model_info,
    text_stream, text_stream_with_usage, tool_use_stream,
};

/// A tool whose output is long enough to trigger micro-compaction's
/// content-collapse threshold, and which attaches opaque `details` — used to
/// prove micro-compaction (a request-projection concern) never touches the
/// canonical transcript item's metadata (M3 test 5), and that full
/// compaction preserves it on items outside the compacted prefix.
struct DetailsTool {
    output: String,
}

impl DetailsTool {
    fn new(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
        }
    }
}

#[async_trait]
impl ToolDefinition for DetailsTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("details_tool")
            .description("test tool: returns long output plus opaque details")
            .input_schema(json!({ "type": "object", "properties": {} }))
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
        Ok(ToolOutput::text(self.output.clone()).with_details(json!({ "marker": "keep-me" })))
    }
}

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
            super::support::StreamScript::Failure(ProviderError::Retryable {
                message: "transient refusal".to_string(),
                delay: None,
            }),
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
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
    let agent_id = agent.id().to_string();
    let mut events = agent.subscribe_events();

    agent
        .run(
            vec![ContentBlock::Text {
                text: "hello".to_string(),
            }],
            RunOptions::default().with_provider_retry(ProviderRetry {
                base_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
                retry_after_cap: Duration::ZERO,
            }),
        )
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
    assert_eq!(requests.len(), 6);
    let first_final_attempt = tool_result_contents(&requests[4]);
    let final_tool_results = tool_result_contents(&requests[5]);
    assert_eq!(
        first_final_attempt, final_tool_results,
        "a transport retry reuses the already-projected request"
    );
    assert_eq!(
        final_tool_results,
        vec![
            "[Previous: used echo_tool]".to_string(),
            "[Previous: used echo_tool]".to_string(),
            long_output.clone(),
            long_output,
        ]
    );

    let elisions = collect_events(&mut events)
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::RequestToolResultsElided { details } => Some(details),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        elisions,
        vec![
            RequestToolResultElision {
                agent_id: agent_id.clone(),
                policy: RequestToolResultElisionPolicy::KeepRecent {
                    configured_keep_recent_tool_results: 2,
                },
                canonical_tool_result_content_bytes: 3 * 140,
                projected_tool_result_content_bytes: 2 * 140 + "[Previous: used echo_tool]".len(),
                results: vec![ElidedToolResult {
                    tool_call_id: "tool-1".to_string(),
                    tool_name: Some("echo_tool".to_string()),
                    is_error: false,
                    canonical_content_kind: ToolResultContentKind::Text,
                    action: ToolResultElisionAction::Marker,
                    canonical_content_bytes: 140,
                    projected_content_bytes: "[Previous: used echo_tool]".len(),
                }],
            },
            RequestToolResultElision {
                agent_id,
                policy: RequestToolResultElisionPolicy::KeepRecent {
                    configured_keep_recent_tool_results: 2,
                },
                canonical_tool_result_content_bytes: 4 * 140,
                projected_tool_result_content_bytes: 2 * 140
                    + 2 * "[Previous: used echo_tool]".len(),
                results: vec![
                    ElidedToolResult {
                        tool_call_id: "tool-1".to_string(),
                        tool_name: Some("echo_tool".to_string()),
                        is_error: false,
                        canonical_content_kind: ToolResultContentKind::Text,
                        action: ToolResultElisionAction::Marker,
                        canonical_content_bytes: 140,
                        projected_content_bytes: "[Previous: used echo_tool]".len(),
                    },
                    ElidedToolResult {
                        tool_call_id: "tool-2".to_string(),
                        tool_name: Some("echo_tool".to_string()),
                        is_error: false,
                        canonical_content_kind: ToolResultContentKind::Text,
                        action: ToolResultElisionAction::Marker,
                        canonical_content_bytes: 140,
                        projected_content_bytes: "[Previous: used echo_tool]".len(),
                    },
                ],
            },
        ]
    );
}

#[tokio::test]
async fn byte_budget_projects_once_per_logical_request_and_never_registers_a_reader() {
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
            super::support::StreamScript::Failure(ProviderError::Retryable {
                message: "transient refusal".to_string(),
                delay: None,
            }),
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
                    // Budget mode wins; this deliberately conflicting legacy
                    // value would otherwise marker-elide every result.
                    keep_recent_tool_results: 0,
                    projected_tool_result_budget: Some(ProjectedToolResultBudget {
                        max_bytes: 300,
                        prioritize_recent_results: 1,
                        max_preview_bytes: 60,
                    }),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = agent.subscribe_events();

    agent
        .run(
            vec![ContentBlock::text("hello")],
            RunOptions::default().with_provider_retry(ProviderRetry {
                base_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
                retry_after_cap: Duration::ZERO,
            }),
        )
        .await
        .unwrap();

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 6);
    assert_eq!(requests[4].messages, requests[5].messages);
    assert!(!tool_names(&requests[5]).contains("read_tool_result"));
    assert_eq!(
        agent.history()[2],
        Message::user(ContentBlock::ToolResult {
            tool_use_id: "tool-1".to_string(),
            content: long_output.into(),
            is_error: false,
        }),
        "the canonical transcript remains unchanged"
    );

    let elisions = collect_events(&mut events)
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::RequestToolResultsElided { details } => Some(details),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        elisions.len(),
        2,
        "estimation and the transport retry emit no duplicate"
    );
    for details in &elisions {
        assert_eq!(
            details.policy,
            RequestToolResultElisionPolicy::ByteBudget {
                configured_max_bytes: 300,
                configured_prioritize_recent_results: 1,
                configured_max_preview_bytes: 60,
            }
        );
        assert!(details.projected_tool_result_content_bytes <= 300);
    }
    assert_eq!(elisions[0].canonical_tool_result_content_bytes, 3 * 140);
    assert_eq!(elisions[0].projected_tool_result_content_bytes, 260);
    assert_eq!(
        elisions[0]
            .results
            .iter()
            .map(|result| result.tool_call_id.as_str())
            .collect::<Vec<_>>(),
        vec!["tool-1", "tool-2"]
    );
    assert_eq!(elisions[1].canonical_tool_result_content_bytes, 4 * 140);
    assert_eq!(elisions[1].projected_tool_result_content_bytes, 300);
    assert_eq!(
        elisions[1]
            .results
            .iter()
            .map(|result| result.tool_call_id.as_str())
            .collect::<Vec<_>>(),
        vec!["tool-1", "tool-2", "tool-3"]
    );
}

// M3 test 5: micro-compaction rewrites old tool results only in the
// *request projection* (`projected_tool_result_history`, a fresh clone of the
// transcript's `Message`s built on every `stream_turn`) — it never touches
// the canonical `TranscriptItem`s themselves, so the details a host attached
// to the collapsed call survive on the stored item even though the outgoing
// request no longer carries the original content.
#[tokio::test]
async fn micro_compaction_leaves_stored_item_details_intact() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let long_output = "x".repeat(140);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "tool-1", "details_tool", r#"{}"#),
            tool_use_stream(&model.id, "tool-2", "details_tool", r#"{}"#),
            tool_use_stream(&model.id, "tool-3", "details_tool", r#"{}"#),
            tool_use_stream(&model.id, "tool-4", "details_tool", r#"{}"#),
            text_stream(&model.id, "done"),
        ],
    );
    let provider_handle = provider.clone();

    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(DetailsTool::new(long_output.clone()))
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

    // The outgoing request for the final round collapsed the two oldest
    // tool results (tool-1, tool-2) to a placeholder — proof micro-compaction
    // actually ran.
    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 5);
    let final_tool_results = tool_result_contents(&requests[4]);
    assert_eq!(
        final_tool_results,
        vec![
            "[Previous: used details_tool]".to_string(),
            "[Previous: used details_tool]".to_string(),
            long_output.clone(),
            long_output,
        ]
    );

    // The canonical transcript item for the collapsed tool-1 call still
    // carries its full details, untouched by the request-side collapse.
    let item = agent
        .transcript()
        .items()
        .iter()
        .find(|item| {
            matches!(
                item.kind,
                TranscriptKind::ToolExchange {
                    tool_use_id: Some(ref id),
                    ..
                } if id == "tool-1"
            )
        })
        .expect("tool-1's transcript item is still present");
    assert_eq!(item.detail("tool-1"), Some(&json!({ "marker": "keep-me" })));
}

// M3 (spec A6 / plan M5 prerequisite, not the M5 guarantee itself): a
// compacted-away prefix is summarized away, but the tail — the last
// assistant tool_use + its tool result — is copied into the replacement
// transcript verbatim (`replacement.extend_from_slice`,
// `src/compaction.rs::StandardCompactionEngine::compact`), so a
// details-bearing item in that tail keeps its metadata "for free" through
// `TranscriptItem`'s derived `Clone`. This is a cheap sanity check on that
// existing copy path, not the exhaustive metadata-preservation contract
// (ADR-0001 §6), which is a separate, later slice.
#[tokio::test]
async fn auto_compaction_preserves_details_on_tail_items() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "call-1", "details_tool", r#"{}"#),
            text_stream(&model.id, "summary"),
            text_stream(&model.id, "after compact"),
        ],
    );
    let transcript_dir = temp_dir("details-preserve-compact");

    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(DetailsTool::new("tool result"))
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

    agent
        .send(vec![ContentBlock::Text {
            text: "run the details tool".to_string(),
        }])
        .await
        .unwrap();

    let compaction = collect_events(&mut events)
        .into_iter()
        .find_map(|event| match event {
            AgentEvent::ContextCompacted { details } => Some(details),
            _ => None,
        })
        .expect("expected a compaction event");
    assert_eq!(compaction.trigger, CompactionTrigger::Auto);
    assert_eq!(
        compaction.preserved_items, 2,
        "the assistant tool_use and its tool result stay in the tail"
    );

    let item = agent
        .transcript()
        .items()
        .iter()
        .find(|item| matches!(item.kind, TranscriptKind::ToolExchange { .. }))
        .expect("compaction preserved the tool exchange item in the tail");
    assert_eq!(item.detail("call-1"), Some(&json!({ "marker": "keep-me" })));
}

#[tokio::test]
async fn auto_compaction_persists_transcript_and_rewrites_history() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let summary_usage = TokenUsage {
        input_tokens: Some(40),
        output_tokens: Some(9),
        total_tokens: Some(49),
        cache_read_input_tokens: Some(7),
        cache_creation_input_tokens: Some(3),
        reasoning_tokens: Some(2),
        thoughts_tokens: Some(1),
        tool_input_tokens: Some(5),
    };
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            text_stream(&model.id, "first done"),
            text_stream_with_usage(&model.id, "summary", summary_usage),
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
    let run_options = RunOptions::default();
    let observed_options = run_options.clone();
    agent
        .run(
            vec![ContentBlock::Text {
                text: "second".to_string(),
            }],
            run_options,
        )
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

    let events = collect_events(&mut events);
    let compaction = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ContextCompacted { details } => Some(details.clone()),
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
    assert_eq!(
        usage_report_fields(&events),
        vec![[40, 9, 7, 3, 2, 1]],
        "local summarizer usage reaches the ordinary UsageReport contract"
    );
    assert_eq!(
        observed_options.reported_tokens(),
        49,
        "auto-compaction charges its run's shared input-plus-output counter"
    );
}

#[tokio::test]
async fn compact_tool_compacts_history_and_continues() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let summary_usage = TokenUsage {
        input_tokens: Some(13),
        output_tokens: Some(4),
        cache_read_input_tokens: Some(2),
        reasoning_tokens: Some(1),
        ..Default::default()
    };
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "compact-1", "compact", "{}"),
            text_stream_with_usage(&model.id, "summary", summary_usage),
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

    let run_options = RunOptions::default();
    let observed_options = run_options.clone();
    agent
        .run(
            vec![ContentBlock::Text {
                text: "please compact".to_string(),
            }],
            run_options,
        )
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

    let events = collect_events(&mut events);
    let compaction = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ContextCompacted { details } => Some(details.clone()),
            _ => None,
        })
        .expect("expected compaction event");
    assert_eq!(compaction.trigger, CompactionTrigger::Manual);
    assert_eq!(compaction.replaced_items, 1);
    assert_eq!(compaction.preserved_items, 1);
    assert_eq!(compaction.preserved_user_turns, 1);
    assert_eq!(compaction.preserved_delegation_results, 0);
    assert_eq!(compaction.resulting_transcript_len, 3);
    assert_eq!(usage_report_fields(&events), vec![[13, 4, 2, 0, 1, 0]]);
    assert_eq!(
        observed_options.reported_tokens(),
        17,
        "the compact intrinsic charges the run that executed it"
    );
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
    let remote_usage = TokenUsage {
        input_tokens: Some(60),
        output_tokens: Some(8),
        total_tokens: Some(68),
        cache_read_input_tokens: Some(11),
        cache_creation_input_tokens: Some(3),
        reasoning_tokens: Some(4),
        thoughts_tokens: Some(2),
        tool_input_tokens: Some(1),
    };
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
            usage: Some(remote_usage),
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

    let events = collect_events(&mut events);
    let compaction = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ContextCompacted { details } => Some(details.clone()),
            _ => None,
        })
        .expect("expected compaction event");
    assert_eq!(compaction.mode, CompactionExecutionMode::Remote);
    assert_eq!(usage_report_fields(&events), vec![[60, 8, 11, 3, 4, 2]]);
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

    let events = collect_events(&mut events);
    let compaction = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ContextCompacted { details } => Some(details.clone()),
            _ => None,
        })
        .expect("expected compaction event");
    assert_eq!(compaction.mode, CompactionExecutionMode::Local);
    assert!(
        usage_report_fields(&events).is_empty(),
        "providers that report no usage must not synthesize a zero report"
    );
}

#[tokio::test]
async fn remote_compaction_falls_back_to_local_on_empty_remote_response() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let local_usage = TokenUsage {
        input_tokens: Some(21),
        output_tokens: Some(5),
        cache_read_input_tokens: Some(2),
        cache_creation_input_tokens: Some(1),
        reasoning_tokens: Some(3),
        thoughts_tokens: Some(4),
        ..Default::default()
    };
    // Provider advertises remote support but returns an empty response — compact_remotely
    // returns Ok(None) which triggers a local fallback.
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            text_stream(&model.id, "first done"),
            text_stream_with_usage(&model.id, "summary", local_usage),
            text_stream(&model.id, "second done"),
        ],
    )
    .with_capabilities(ProviderCapabilities {
        supports_history_compaction: true,
        ..Default::default()
    });

    provider
        .push_compact_response(Ok(CompactionResponse {
            output: vec![],
            usage: Some(TokenUsage::default()),
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

    let events = collect_events(&mut events);
    let compaction = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ContextCompacted { details } => Some(details.clone()),
            _ => None,
        })
        .expect("expected compaction event");
    assert_eq!(compaction.mode, CompactionExecutionMode::Local);
    assert_eq!(
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ContextCompacted { .. } => Some("context"),
                AgentEvent::UsageReport { .. } => Some("usage"),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["context", "usage", "usage"],
        "the applied compaction is announced before its usage samples"
    );
    assert_eq!(
        usage_report_fields(&events),
        vec![[0, 0, 0, 0, 0, 0], [21, 5, 2, 1, 3, 4]],
        "reported-empty remote usage precedes local fallback usage"
    );
}

#[tokio::test]
async fn manual_session_compaction_maps_completion_before_usage() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            text_stream(&model.id, "first done"),
            text_stream(&model.id, "second done"),
            text_stream_with_usage(
                &model.id,
                "summary",
                TokenUsage {
                    input_tokens: Some(31),
                    output_tokens: Some(7),
                    cache_read_input_tokens: Some(5),
                    cache_creation_input_tokens: Some(2),
                    reasoning_tokens: Some(3),
                    thoughts_tokens: Some(1),
                    ..Default::default()
                },
            ),
        ],
    );
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut session = runtime
        .create_session_with_config(
            "manual-usage",
            model,
            AgentConfig {
                compaction: CompactionConfig {
                    auto_compact_threshold_tokens: None,
                    transcript_dir: temp_dir("manual-session-usage"),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .expect("create session");

    session
        .append_turn(vec![ContentBlock::text("first")])
        .await
        .expect("first turn");
    session
        .append_turn(vec![ContentBlock::text("second")])
        .await
        .expect("second turn");
    let mut events = session.subscribe();

    session
        .compact(None)
        .await
        .expect("manual compaction")
        .expect("compaction details");

    let relevant = collect_session_events(&mut events)
        .into_iter()
        .filter(|event| {
            matches!(
                event,
                SessionEvent::CompactionStarted { .. }
                    | SessionEvent::CompactionCompleted { .. }
                    | SessionEvent::UsageReport { .. }
            )
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        relevant.first(),
        Some(SessionEvent::CompactionStarted { .. })
    ));
    assert!(matches!(
        relevant.get(1),
        Some(SessionEvent::CompactionCompleted { .. })
    ));
    assert!(matches!(
        relevant.get(2),
        Some(SessionEvent::UsageReport {
            input_tokens: 31,
            output_tokens: 7,
            cache_read_tokens: 5,
            cache_creation_tokens: 2,
            reasoning_tokens: 3,
            thoughts_tokens: 1,
            ..
        })
    ));
    assert_eq!(
        relevant.len(),
        3,
        "Basis receives the existing compaction pair followed by ordinary usage"
    );
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
    let store = temp_persistent_store("resume-after-compact");

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
    clear_leases(&store);

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
        assert_eq!(
            resumed_agents.len(),
            1,
            "expected exactly one resumed agent"
        );
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

fn temp_persistent_store(label: &str) -> PersistentStore {
    let unique = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "mentra-runtime-compact-{label}-{timestamp}-{unique}.store"
    ));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create temp dir");
    }
    PersistentStore::new(path)
}

/// Simulates the previous lease holder having died, so a second runtime on
/// the same store can resume: a SQL DELETE for the SQLite store, dropping
/// the held OS locks for the file store.
#[cfg(feature = "store-sqlite")]
fn clear_leases(store: &PersistentStore) {
    let conn = rusqlite::Connection::open(store.path()).expect("open store");
    conn.execute("DELETE FROM leases", [])
        .expect("clear leases");
}

#[cfg(not(feature = "store-sqlite"))]
fn clear_leases(store: &PersistentStore) {
    store.release_all_leases();
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

fn usage_report_fields(events: &[AgentEvent]) -> Vec<[u64; 6]> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::UsageReport {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
                reasoning_tokens,
                thoughts_tokens,
            } => Some([
                *input_tokens,
                *output_tokens,
                *cache_read_tokens,
                *cache_creation_tokens,
                *reasoning_tokens,
                *thoughts_tokens,
            ]),
            _ => None,
        })
        .collect()
}

fn collect_session_events(
    receiver: &mut tokio::sync::broadcast::Receiver<SessionEvent>,
) -> Vec<SessionEvent> {
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

#[tokio::test]
async fn a_provider_that_says_the_request_is_too_long_gets_a_compacted_one() {
    // The threshold is an estimate of what will fit; the provider's refusal is
    // the authoritative answer. A run whose only problem is its own length
    // should not die of it.
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            super::support::StreamScript::Failure(ProviderError::ContextLengthExceeded {
                status: reqwest::StatusCode::BAD_REQUEST,
                body: "prompt is too long: 215000 tokens > 200000 maximum".to_string(),
            }),
            text_stream(&model.id, "answered after compacting"),
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
            usage: None,
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
                    // Auto-compaction is off, so nothing but the provider's
                    // refusal can have triggered the compaction.
                    auto_compact_threshold_tokens: None,
                    mode: CompactionMode::PreferRemote,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();

    let message = agent
        .send(vec![ContentBlock::Text {
            text: "hello".to_string(),
        }])
        .await
        .expect("the run recovers instead of failing");

    assert_eq!(message.text(), "answered after compacting");
}

#[tokio::test]
async fn overflow_recovery_survives_a_store_that_refuses_the_summary_write() {
    // The file store refuses long-term memory, and the compaction summary is
    // a long-term memory record. A recovery whose compaction is already
    // applied must not be failed by that refusal — it used to be, which
    // turned "context overflow on the file store" into a dead run instead of
    // a recovered one, with the ContextCompacted announcement skipped too.
    let model = model_info("model", BuiltinProvider::Anthropic);
    let long_output = "x".repeat(400);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            // First turn builds enough history that the overflow compaction
            // has a prefix to replace.
            tool_use_stream(&model.id, "tool-1", "echo_tool", r#"{"value":"one"}"#),
            text_stream(&model.id, "first answer"),
            // Second turn: the provider refuses the request as too long, and
            // accepts the retry sent after compacting.
            super::support::StreamScript::Failure(ProviderError::ContextLengthExceeded {
                status: reqwest::StatusCode::BAD_REQUEST,
                body: "prompt is too long".to_string(),
            }),
            text_stream(&model.id, "answered after compacting"),
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
            usage: None,
        }))
        .await;

    let runtime = Runtime::empty_builder()
        .with_store(crate::runtime::FileRuntimeStore::new(temp_dir(
            "file-store-overflow",
        )))
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
                    auto_compact_threshold_tokens: None,
                    mode: CompactionMode::PreferRemote,
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
        .expect("first turn builds history");

    let mut events = agent.subscribe_events();
    let message = agent
        .send(vec![ContentBlock::Text {
            text: "and again".to_string(),
        }])
        .await
        .expect("the run recovers even though the summary write is refused");
    assert_eq!(message.text(), "answered after compacting");

    let collected = collect_events(&mut events);
    let compacted = collected
        .iter()
        .any(|event| matches!(event, AgentEvent::ContextCompacted { .. }));
    assert!(
        compacted,
        "the applied compaction is still announced, got {collected:?}"
    );
}

#[tokio::test]
async fn a_second_overflow_after_compacting_is_not_retried_again() {
    // Compacting twice does not make a request that is still too long fit, and
    // a loop here would grind the transcript away one summary at a time.
    let model = model_info("model", BuiltinProvider::Anthropic);
    let too_long = || {
        super::support::StreamScript::Failure(ProviderError::ContextLengthExceeded {
            status: reqwest::StatusCode::BAD_REQUEST,
            body: "prompt is too long".to_string(),
        })
    };
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![too_long(), too_long(), text_stream(&model.id, "never run")],
    )
    .with_capabilities(ProviderCapabilities {
        supports_history_compaction: true,
        ..Default::default()
    });

    provider
        .push_compact_response(Ok(CompactionResponse {
            output: vec![CompactionInputItem::CompactionSummary {
                content: "Summary".to_string(),
            }],
            usage: None,
        }))
        .await;

    let provider_handle = provider.clone();
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
                    auto_compact_threshold_tokens: None,
                    mode: CompactionMode::PreferRemote,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();

    let result = agent
        .send(vec![ContentBlock::Text {
            text: "hello".to_string(),
        }])
        .await;

    assert!(result.is_err(), "the second refusal ends the run");
    assert_eq!(
        provider_handle.recorded_requests().await.len(),
        2,
        "one compaction, one retry, and then it stops"
    );
}

/// Answers the first request normally and then never answers another, so a
/// test can reach a real compaction — which needs a completed exchange to
/// summarize — and then hold its provider call open.
struct AnswersOnceThenBlocksProvider {
    model: crate::ModelInfo,
    requests: Arc<AtomicU64>,
}

#[async_trait]
impl crate::provider::Provider for AnswersOnceThenBlocksProvider {
    fn descriptor(&self) -> crate::provider::ProviderDescriptor {
        crate::provider::ProviderDescriptor::new(self.model.provider.clone())
    }

    async fn list_models(&self) -> Result<Vec<crate::ModelInfo>, ProviderError> {
        Ok(vec![self.model.clone()])
    }

    async fn stream(
        &self,
        _request: Request<'_>,
    ) -> Result<crate::provider::ProviderEventStream, ProviderError> {
        if self.requests.fetch_add(1, Ordering::SeqCst) == 0 {
            return Ok(crate::provider::provider_event_stream_from_response(
                crate::provider::Response {
                    id: "first".to_string(),
                    model: self.model.id.clone(),
                    role: Role::Assistant,
                    content: vec![ContentBlock::text("first response")],
                    stop_reason: None,
                    usage: None,
                },
            ));
        }
        std::future::pending().await
    }
}

/// Answers the first request normally; every request after it fails retryably
/// and trips `cancellation` on the way out — the shape of a run cancelled
/// between two compaction attempts.
struct AnswersOnceThenCancelsProvider {
    model: crate::ModelInfo,
    requests: Arc<AtomicU64>,
    cancellation: crate::runtime::CancellationToken,
}

#[async_trait]
impl crate::provider::Provider for AnswersOnceThenCancelsProvider {
    fn descriptor(&self) -> crate::provider::ProviderDescriptor {
        crate::provider::ProviderDescriptor::new(self.model.provider.clone())
    }

    async fn list_models(&self) -> Result<Vec<crate::ModelInfo>, ProviderError> {
        Ok(vec![self.model.clone()])
    }

    async fn stream(
        &self,
        _request: Request<'_>,
    ) -> Result<crate::provider::ProviderEventStream, ProviderError> {
        if self.requests.fetch_add(1, Ordering::SeqCst) == 0 {
            return Ok(crate::provider::provider_event_stream_from_response(
                crate::provider::Response {
                    id: "first".to_string(),
                    model: self.model.id.clone(),
                    role: Role::Assistant,
                    content: vec![ContentBlock::text("first response")],
                    stop_reason: None,
                    usage: None,
                },
            ));
        }
        self.cancellation.cancel();
        Err(ProviderError::Retryable {
            message: "summarizer is briefly unavailable".to_string(),
            delay: None,
        })
    }
}

/// Spawns an agent that auto-compacts before every model request.
fn compacting_agent<P: crate::provider::Provider + 'static>(
    provider: P,
    model: crate::ModelInfo,
    label: &str,
) -> crate::Agent {
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                compaction: CompactionConfig {
                    auto_compact_threshold_tokens: Some(1),
                    transcript_dir: temp_dir(label),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .expect("spawn agent")
}

// Virtual time, for the reason given on the retry-delay test below: the
// summarizer here never answers, so what is measured is how long the run waits
// before giving up on it.
#[tokio::test(start_paused = true)]
async fn cancelling_a_run_stops_a_compaction_already_in_flight() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let requests = Arc::new(AtomicU64::new(0));
    let mut agent = compacting_agent(
        AnswersOnceThenBlocksProvider {
            model: model.clone(),
            requests: Arc::clone(&requests),
        },
        model,
        "cancel-in-flight-compaction",
    );

    // One completed exchange, so the next turn has something older than its
    // protected tail to summarize.
    agent
        .send(vec![ContentBlock::text("first turn")])
        .await
        .expect("first turn");

    let cancellation = crate::runtime::CancellationToken::default();
    let canceller = tokio::spawn({
        let cancellation = cancellation.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancellation.cancel();
        }
    });

    let started = tokio::time::Instant::now();
    let result = agent
        .run(
            vec![ContentBlock::text("second turn")],
            RunOptions {
                cancellation: Some(cancellation),
                ..Default::default()
            },
        )
        .await;
    canceller.await.expect("canceller task");

    assert!(
        matches!(result, Err(RuntimeError::Cancelled)),
        "a cancel during compaction must end the run as cancelled, not wait for \
         the summarizer; got {result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "the run must not sit on a provider call that never answers; waited {:?}",
        started.elapsed()
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "the first turn, then the abandoned compaction; the second turn never \
         reached the model"
    );
}

#[tokio::test]
async fn a_deadline_reached_during_compaction_ends_the_run() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let requests = Arc::new(AtomicU64::new(0));
    let mut agent = compacting_agent(
        AnswersOnceThenBlocksProvider {
            model: model.clone(),
            requests: Arc::clone(&requests),
        },
        model,
        "deadline-during-compaction",
    );

    agent
        .send(vec![ContentBlock::text("first turn")])
        .await
        .expect("first turn");

    // Real time, deliberately: the deadline is a `SystemTime`, which
    // `start_paused` does not virtualize. The margin is generous on purpose —
    // the deadline has to outlast this run's setup (checkpoint and journal
    // writes) so that the round-boundary check passes and the compaction is
    // actually reached, which is the thing under test. The run does not wait
    // it out: the summarizer never answers, so the deadline is what ends it.
    let result = agent
        .run(
            vec![ContentBlock::text("second turn")],
            RunOptions {
                deadline: Some(SystemTime::now() + Duration::from_secs(2)),
                ..Default::default()
            },
        )
        .await;

    assert!(
        matches!(result, Err(RuntimeError::DeadlineExceeded)),
        "got {result:?}"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "the first turn, then the compaction the deadline cut short"
    );
}

// Virtual time: the claim is precisely "the 500 ms retry delay is not
// waited out", and under `start_paused` the clock advances only for sleeps the
// code actually asks for — so the elapsed value below is the delay itself, not
// the test machine's load.
#[tokio::test(start_paused = true)]
async fn a_cancellation_between_compaction_attempts_ends_the_run_instead_of_degrading() {
    // The auto-compaction retry loop deliberately swallows a failure and
    // carries on with micro-compaction only. A cancellation is not a failure
    // to degrade past: swallowing it would let the turn proceed after the
    // caller asked for it to stop.
    let model = model_info("model", BuiltinProvider::Anthropic);
    let requests = Arc::new(AtomicU64::new(0));
    let cancellation = crate::runtime::CancellationToken::default();
    let mut agent = compacting_agent(
        AnswersOnceThenCancelsProvider {
            model: model.clone(),
            requests: Arc::clone(&requests),
            cancellation: cancellation.clone(),
        },
        model,
        "cancel-between-attempts",
    );

    agent
        .send(vec![ContentBlock::text("first turn")])
        .await
        .expect("first turn");

    let started = tokio::time::Instant::now();
    let result = agent
        .run(
            vec![ContentBlock::text("second turn")],
            RunOptions {
                cancellation: Some(cancellation),
                ..Default::default()
            },
        )
        .await;

    assert!(
        matches!(result, Err(RuntimeError::Cancelled)),
        "got {result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "the loop must notice the cancellation instead of sleeping out its \
         500 ms retry delay; waited {:?}",
        started.elapsed()
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "the first turn, then one failed compaction attempt: a cancelled run \
         does not spend the remaining two"
    );
}
