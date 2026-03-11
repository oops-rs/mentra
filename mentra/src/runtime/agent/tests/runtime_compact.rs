use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    provider::{ContentBlockDelta, ContentBlockStart, ProviderEvent, Request},
    ContentBlock, Message, ModelProviderKind, Role,
    runtime::{
        AgentConfig, AgentEvent, ContextCompactionConfig, ContextCompactionTrigger, Runtime,
    },
};

use super::support::{ScriptedProvider, StaticTool, StreamScript, model_info, ok_stream};

#[tokio::test]
async fn micro_compaction_only_rewrites_old_tool_results_in_requests() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let long_output = "x".repeat(140);
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
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
                context_compaction: ContextCompactionConfig {
                    keep_recent_tool_results: 2,
                    auto_compact_threshold_tokens: None,
                    ..ContextCompactionConfig::default()
                },
                ..AgentConfig::default()
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
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tool-1".to_string(),
                content: long_output.clone(),
                is_error: false,
            }],
        }
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
    let model = model_info("model", ModelProviderKind::Anthropic);
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
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
                context_compaction: ContextCompactionConfig {
                    auto_compact_threshold_tokens: Some(1),
                    transcript_dir: transcript_dir.clone(),
                    ..ContextCompactionConfig::default()
                },
                ..AgentConfig::default()
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

    assert_eq!(agent.history().len(), 3);
    assert_eq!(agent.history()[0].role, Role::User);
    assert_eq!(
        message_text(&agent.history()[0]),
        "[Compressed context]\n\nsummary"
    );

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
    assert_eq!(
        message_text(&requests[2].messages[0]),
        "[Compressed context]\n\nsummary"
    );

    let compaction = collect_events(&mut events)
        .into_iter()
        .find_map(|event| match event {
            AgentEvent::ContextCompacted { details } => Some(details),
            _ => None,
        })
        .expect("expected compaction event");
    assert_eq!(compaction.trigger, ContextCompactionTrigger::Auto);
    assert_eq!(compaction.replaced_messages, 2);
    assert_eq!(compaction.preserved_messages, 1);
    assert_eq!(compaction.resulting_history_len, 2);
    assert!(compaction.transcript_path.starts_with(&transcript_dir));
}

#[tokio::test]
async fn compact_tool_compacts_history_and_continues() {
    let model = model_info("model", ModelProviderKind::Anthropic);
    let provider = ScriptedProvider::new(
        ModelProviderKind::Anthropic,
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
                context_compaction: ContextCompactionConfig {
                    auto_compact_threshold_tokens: None,
                    transcript_dir,
                    ..ContextCompactionConfig::default()
                },
                ..AgentConfig::default()
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

    assert_eq!(agent.history().len(), 4);
    assert_eq!(
        message_text(&agent.history()[0]),
        "[Compressed context]\n\nsummary"
    );
    assert!(matches!(
        &agent.history()[2].content[0],
        ContentBlock::ToolResult { is_error: false, content, .. }
            if content.starts_with("Context compacted. Transcript saved to ")
    ));

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 3);
    assert!(requests[1].tools.is_empty());
    assert_eq!(requests[1].tool_choice, None);
    assert_eq!(
        message_text(&requests[2].messages[0]),
        "[Compressed context]\n\nsummary"
    );
    assert!(tool_names(&requests[0]).contains("compact"));

    let compaction = collect_events(&mut events)
        .into_iter()
        .find_map(|event| match event {
            AgentEvent::ContextCompacted { details } => Some(details),
            _ => None,
        })
        .expect("expected compaction event");
    assert_eq!(compaction.trigger, ContextCompactionTrigger::Manual);
    assert_eq!(compaction.replaced_messages, 1);
    assert_eq!(compaction.preserved_messages, 1);
    assert_eq!(compaction.resulting_history_len, 2);
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
            ContentBlock::ToolResult { content, .. } => Some(content.clone()),
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
