use serde_json::json;

use crate::{
    provider::model::{
        ContentBlock, ContentBlockDelta, ContentBlockStart, Message, ProviderEvent, Role,
    },
    runtime::{AgentEvent, PendingAssistantTurn},
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
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "Hello".to_string(),
            }],
        }
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
fn pending_turn_rejects_missing_stop_and_malformed_tool_json() {
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
            .is_err()
    );
}
