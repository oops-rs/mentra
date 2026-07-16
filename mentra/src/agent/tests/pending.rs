use serde_json::json;

use crate::{
    ContentBlock, Message, Role,
    agent::{AgentEvent, PendingAssistantTurn},
    provider::{ContentBlockDelta, ContentBlockStart, ProviderEvent, TokenUsage},
    tool::ToolCall,
};

#[test]
fn text_turn_commits_after_message_stop() {
    let mut pending = PendingAssistantTurn::default();

    assert!(
        pending
            .apply(ProviderEvent::MessageStarted {
                id: "msg-1".to_string(),
                model: "model".to_string(),
                role: Role::Assistant,
            })
            .unwrap()
            .is_empty()
    );
    assert!(
        pending
            .apply(ProviderEvent::ContentBlockStarted {
                index: 0,
                kind: ContentBlockStart::Text,
            })
            .unwrap()
            .is_empty()
    );

    let derived = pending
        .apply(ProviderEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::Text("Hello".to_string()),
        })
        .unwrap();
    assert_eq!(
        derived,
        vec![AgentEvent::TextDelta {
            delta: "Hello".to_string(),
            full_text: "Hello".to_string(),
        }]
    );

    pending
        .apply(ProviderEvent::ContentBlockStopped { index: 0 })
        .unwrap();
    pending.apply(ProviderEvent::MessageStopped).unwrap();

    assert_eq!(
        pending.to_message().unwrap(),
        Message::assistant(ContentBlock::text("Hello"))
    );
}

#[test]
fn thinking_turn_emits_text_only_deltas_and_commits_signature_at_block_close() {
    let provenance = crate::ReasoningProvenance {
        provider: crate::ProviderId::new("anthropic-edge"),
        model: "claude-test".to_string(),
        format: crate::ReasoningFormat::AnthropicSigned,
    };
    let mut pending = PendingAssistantTurn::default();
    pending
        .apply(ProviderEvent::MessageStarted {
            id: "msg-thinking".to_string(),
            model: "claude-test".to_string(),
            role: Role::Assistant,
        })
        .unwrap();
    pending
        .apply(ProviderEvent::ContentBlockStarted {
            index: 0,
            kind: ContentBlockStart::Thinking {
                encrypted_content: None,
                id: None,
                provenance: Some(provenance.clone()),
                redacted: false,
            },
        })
        .unwrap();

    assert_eq!(
        pending
            .apply(ProviderEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::ThinkingText("private ".to_string()),
            })
            .unwrap(),
        vec![AgentEvent::ReasoningDelta {
            delta: "private ".to_string(),
            full_text: "private ".to_string(),
        }]
    );
    assert_eq!(
        pending
            .apply(ProviderEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::ThinkingText("chain".to_string()),
            })
            .unwrap(),
        vec![AgentEvent::ReasoningDelta {
            delta: "chain".to_string(),
            full_text: "private chain".to_string(),
        }]
    );
    assert!(
        pending
            .apply(ProviderEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::ThinkingSignature("opaque-signature".to_string()),
            })
            .unwrap()
            .is_empty()
    );
    pending
        .apply(ProviderEvent::ContentBlockStopped { index: 0 })
        .unwrap();
    pending.apply(ProviderEvent::MessageStopped).unwrap();

    assert_eq!(
        pending.to_message().unwrap(),
        Message::assistant(ContentBlock::Thinking {
            thinking: "private chain".to_string(),
            signature: Some("opaque-signature".to_string()),
            encrypted_content: None,
            id: None,
            provenance: Some(provenance),
            redacted: false,
        })
    );
}

#[test]
fn tool_use_turn_emits_ready_event_and_parses_call() {
    let mut pending = PendingAssistantTurn::default();

    pending
        .apply(ProviderEvent::MessageStarted {
            id: "msg-1".to_string(),
            model: "model".to_string(),
            role: Role::Assistant,
        })
        .unwrap();
    pending
        .apply(ProviderEvent::ContentBlockStarted {
            index: 0,
            kind: ContentBlockStart::ToolUse {
                id: "tool-1".to_string(),
                name: "echo_tool".to_string(),
            },
        })
        .unwrap();
    pending
        .apply(ProviderEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::ToolUseInputJson(r#"{"value":"hi"}"#.to_string()),
        })
        .unwrap();

    let derived = pending
        .apply(ProviderEvent::ContentBlockStopped { index: 0 })
        .unwrap();
    assert_eq!(
        derived,
        vec![AgentEvent::ToolUseReady {
            index: 0,
            call: ToolCall {
                id: "tool-1".to_string(),
                name: "echo_tool".to_string(),
                input: json!({ "value": "hi" }),
            },
        }]
    );

    pending.apply(ProviderEvent::MessageStopped).unwrap();
    assert_eq!(pending.ready_tool_calls().unwrap().len(), 1);
}

#[test]
fn pending_turn_rejects_missing_stop_and_recovers_from_malformed_tool_json() {
    let mut text_pending = PendingAssistantTurn::default();
    text_pending
        .apply(ProviderEvent::MessageStarted {
            id: "msg-1".to_string(),
            model: "model".to_string(),
            role: Role::Assistant,
        })
        .unwrap();
    text_pending
        .apply(ProviderEvent::ContentBlockStarted {
            index: 0,
            kind: ContentBlockStart::Text,
        })
        .unwrap();
    text_pending
        .apply(ProviderEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::Text("Hello".to_string()),
        })
        .unwrap();
    text_pending
        .apply(ProviderEvent::ContentBlockStopped { index: 0 })
        .unwrap();
    assert!(text_pending.to_message().is_err());

    let mut tool_pending = PendingAssistantTurn::default();
    tool_pending
        .apply(ProviderEvent::MessageStarted {
            id: "msg-2".to_string(),
            model: "model".to_string(),
            role: Role::Assistant,
        })
        .unwrap();
    tool_pending
        .apply(ProviderEvent::ContentBlockStarted {
            index: 0,
            kind: ContentBlockStart::ToolUse {
                id: "tool-1".to_string(),
                name: "broken_tool".to_string(),
            },
        })
        .unwrap();
    tool_pending
        .apply(ProviderEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::ToolUseInputJson("{".to_string()),
        })
        .unwrap();
    assert!(
        tool_pending
            .apply(ProviderEvent::ContentBlockStopped { index: 0 })
            .unwrap()
            .is_empty()
    );
    tool_pending.apply(ProviderEvent::MessageStopped).unwrap();

    assert!(tool_pending.ready_tool_calls().unwrap().is_empty());
    assert_eq!(tool_pending.invalid_tool_uses().len(), 1);
    assert_eq!(
        tool_pending.to_message().unwrap(),
        Message {
            role: Role::Assistant,
            content: Vec::new(),
        }
    );
}

#[test]
fn pending_turn_tracks_latest_usage_without_affecting_message() {
    let mut pending = PendingAssistantTurn::default();
    pending
        .apply(ProviderEvent::MessageStarted {
            id: "msg-1".to_string(),
            model: "model".to_string(),
            role: Role::Assistant,
        })
        .unwrap();
    pending
        .apply(ProviderEvent::ContentBlockStarted {
            index: 0,
            kind: ContentBlockStart::Text,
        })
        .unwrap();
    pending
        .apply(ProviderEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::Text("Hello".to_string()),
        })
        .unwrap();
    pending
        .apply(ProviderEvent::ContentBlockStopped { index: 0 })
        .unwrap();
    pending
        .apply(ProviderEvent::MessageDelta {
            stop_reason: Some("stop".to_string()),
            usage: Some(TokenUsage {
                input_tokens: Some(12),
                output_tokens: Some(3),
                total_tokens: Some(15),
                ..TokenUsage::default()
            }),
        })
        .unwrap();
    pending.apply(ProviderEvent::MessageStopped).unwrap();

    assert_eq!(
        pending.usage(),
        Some(&TokenUsage {
            input_tokens: Some(12),
            output_tokens: Some(3),
            total_tokens: Some(15),
            ..TokenUsage::default()
        })
    );
    assert_eq!(pending.stop_reason(), Some("stop"));
    assert_eq!(
        pending.to_message().unwrap(),
        Message::assistant(ContentBlock::text("Hello"))
    );
}
