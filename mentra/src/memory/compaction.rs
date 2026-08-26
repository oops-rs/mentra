use std::collections::HashMap;

use crate::{ContentBlock, Message, Role};

const MICRO_COMPACT_MIN_CONTENT_LEN: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MicroCompactedToolResult {
    pub(crate) tool_use_id: String,
    pub(crate) tool_name: Option<String>,
    pub(crate) is_error: bool,
    pub(crate) original_content_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MicroCompactedHistory {
    pub(crate) messages: Vec<Message>,
    pub(crate) elided_tool_results: Vec<MicroCompactedToolResult>,
}

pub(crate) fn micro_compact_history(
    history: &[Message],
    keep_recent: usize,
) -> MicroCompactedHistory {
    if keep_recent == usize::MAX {
        return MicroCompactedHistory {
            messages: history.to_vec(),
            elided_tool_results: Vec::new(),
        };
    }

    let mut compacted = history.to_vec();
    let tool_names = tool_name_index(&compacted);
    let mut tool_results = Vec::new();
    let mut elided_tool_results = Vec::new();

    for (message_index, message) in compacted.iter().enumerate() {
        if message.role != Role::User {
            continue;
        }

        for (block_index, block) in message.content.iter().enumerate() {
            if matches!(block, ContentBlock::ToolResult { .. }) {
                tool_results.push((message_index, block_index));
            }
        }
    }

    if tool_results.len() <= keep_recent {
        return MicroCompactedHistory {
            messages: compacted,
            elided_tool_results,
        };
    }

    let compact_count = tool_results.len() - keep_recent;
    for (message_index, block_index) in tool_results.into_iter().take(compact_count) {
        let Some(ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        }) = compacted[message_index].content.get_mut(block_index)
        else {
            continue;
        };

        if content.len() <= MICRO_COMPACT_MIN_CONTENT_LEN {
            continue;
        }

        let original_content_bytes = content.len();
        let tool_name = tool_names.get(tool_use_id.as_str()).cloned();
        content.clear();
        content.push_str(&format!(
            "[Previous: used {}]",
            tool_name.as_deref().unwrap_or("tool")
        ));
        elided_tool_results.push(MicroCompactedToolResult {
            tool_use_id: tool_use_id.clone(),
            tool_name,
            is_error: *is_error,
            original_content_bytes,
        });
    }

    MicroCompactedHistory {
        messages: compacted,
        elided_tool_results,
    }
}

/// Estimates how many tokens a request carrying `messages` and `system` costs.
///
/// This is the same estimate the runtime's own auto-compaction is evaluated
/// against, exposed because a host has no other way to report context usage:
/// a provider reports what a turn *cost* only after it has run, and a usage
/// bar has to say what the next turn will cost before it is sent. It is an
/// estimate — cheap, provider-neutral, and never a substitute for the token
/// counts a response reports.
pub fn estimated_request_tokens(messages: &[Message], system: Option<&str>) -> usize {
    let mut estimated =
        estimated_tokens_for_str(&serde_json::to_string(messages).unwrap_or_default());
    if let Some(system) = system {
        estimated += estimated_tokens_for_str(system);
    }
    estimated
}

pub(crate) fn required_tail_start_for_continuation(history: &[Message]) -> usize {
    let Some(last_index) = history.len().checked_sub(1) else {
        return 0;
    };
    let last_message = &history[last_index];

    if last_message.role == Role::User
        && last_message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        && last_index > 0
        && history[last_index - 1].role == Role::Assistant
        && history[last_index - 1]
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
    {
        last_index - 1
    } else {
        last_index
    }
}

fn tool_name_index(history: &[Message]) -> HashMap<String, String> {
    let mut tool_names = HashMap::new();

    for message in history {
        for block in &message.content {
            if let ContentBlock::ToolUse { id, name, .. } = block {
                tool_names.insert(id.clone(), name.clone());
            }
        }
    }

    tool_names
}

fn estimated_tokens_for_str(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ContentBlock;

    #[test]
    fn the_estimator_a_host_reports_with_is_the_one_compaction_uses() {
        // A usage bar has to say what the *next* turn costs, before it is sent;
        // a provider only reports what a turn cost once it has run. This is the
        // only number available at that point, and it is the same one
        // auto-compaction is evaluated against.
        let history = vec![Message::user(ContentBlock::text("a".repeat(400)))];

        let without_system = estimated_request_tokens(&history, None);
        let with_system = estimated_request_tokens(&history, Some(&"b".repeat(400)));

        assert!(without_system > 0);
        assert!(
            with_system > without_system,
            "the system prompt is part of what the request will cost"
        );
    }

    #[test]
    fn keeping_every_tool_result_copies_the_history_unchanged() {
        let history = vec![
            Message::user(ContentBlock::text("go")),
            Message::user(ContentBlock::ToolResult {
                tool_use_id: "call".to_string(),
                content: crate::tool::ToolResultContent::text("x".repeat(500)),
                is_error: false,
            }),
        ];

        let projected = micro_compact_history(&history, usize::MAX);

        assert_eq!(projected.messages, history);
        assert!(projected.elided_tool_results.is_empty());
    }

    #[test]
    fn elision_report_contains_only_results_that_were_rewritten() {
        let long = "x".repeat(101);
        let short = "y".repeat(100);
        let history = vec![
            Message::assistant(ContentBlock::ToolUse {
                id: "long-old".to_string(),
                name: "read_file".to_string(),
                input: serde_json::json!({}),
            }),
            Message::user(ContentBlock::ToolResult {
                tool_use_id: "long-old".to_string(),
                content: crate::tool::ToolResultContent::text(long.clone()),
                is_error: true,
            }),
            Message::assistant(ContentBlock::ToolUse {
                id: "short-old".to_string(),
                name: "stat_file".to_string(),
                input: serde_json::json!({}),
            }),
            Message::user(ContentBlock::ToolResult {
                tool_use_id: "short-old".to_string(),
                content: crate::tool::ToolResultContent::text(short.clone()),
                is_error: false,
            }),
            Message::assistant(ContentBlock::ToolUse {
                id: "long-recent".to_string(),
                name: "read_file".to_string(),
                input: serde_json::json!({}),
            }),
            Message::user(ContentBlock::ToolResult {
                tool_use_id: "long-recent".to_string(),
                content: crate::tool::ToolResultContent::text(long),
                is_error: false,
            }),
        ];

        let projected = micro_compact_history(&history, 1);

        assert_eq!(
            projected.elided_tool_results,
            vec![MicroCompactedToolResult {
                tool_use_id: "long-old".to_string(),
                tool_name: Some("read_file".to_string()),
                is_error: true,
                original_content_bytes: 101,
            }]
        );
        assert_eq!(
            projected.messages[1].content,
            vec![ContentBlock::ToolResult {
                tool_use_id: "long-old".to_string(),
                content: crate::tool::ToolResultContent::text("[Previous: used read_file]"),
                is_error: true,
            }]
        );
        assert_eq!(
            projected.messages[3].content, history[3].content,
            "the selected 100-byte result stays whole"
        );
        assert_eq!(
            projected.messages[5].content, history[5].content,
            "the recent suffix stays whole"
        );
    }
}
