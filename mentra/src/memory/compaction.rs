use std::collections::HashMap;

use crate::{
    ContentBlock, Message, Role,
    agent::{
        ElidedToolResult, ProjectedToolResultBudget, RequestToolResultElisionPolicy,
        ToolResultContentKind, ToolResultElisionAction,
    },
    tool::ToolResultContent,
};

const MICRO_COMPACT_MIN_CONTENT_LEN: usize = 100;
const OMITTED_ELLIPSIS: &str = "…";
const PREVIEW_SEPARATOR: &str = "\n…[omitted]…\n";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolResultProjectionReport {
    pub(crate) policy: RequestToolResultElisionPolicy,
    pub(crate) canonical_tool_result_content_bytes: usize,
    pub(crate) projected_tool_result_content_bytes: usize,
    pub(crate) results: Vec<ElidedToolResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectedToolResultHistory {
    pub(crate) messages: Vec<Message>,
    pub(crate) report: Option<ToolResultProjectionReport>,
}

#[derive(Debug, Clone)]
struct ToolResultLocation {
    message_index: usize,
    block_index: usize,
    tool_use_id: String,
    tool_name: Option<String>,
    is_error: bool,
}

pub(crate) fn project_tool_result_history(
    history: &[Message],
    keep_recent: usize,
    budget: Option<ProjectedToolResultBudget>,
) -> ProjectedToolResultHistory {
    match budget {
        Some(budget) => budget_tool_result_history(history, budget),
        None => micro_compact_history(history, keep_recent),
    }
}

pub(crate) fn micro_compact_history(
    history: &[Message],
    keep_recent: usize,
) -> ProjectedToolResultHistory {
    if keep_recent == usize::MAX {
        return ProjectedToolResultHistory {
            messages: history.to_vec(),
            report: None,
        };
    }

    let mut compacted = history.to_vec();
    let tool_names = tool_name_index(&compacted);
    let mut tool_results = Vec::new();
    let mut elided_tool_results = Vec::new();
    let canonical_tool_result_content_bytes = tool_result_content_bytes(history);

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
        return ProjectedToolResultHistory {
            messages: compacted,
            report: None,
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

        let canonical_content_kind = content_kind(content);
        let canonical_content_bytes = content.len();
        let tool_name = tool_names.get(tool_use_id.as_str()).cloned();
        content.clear();
        content.push_str(&format!(
            "[Previous: used {}]",
            tool_name.as_deref().unwrap_or("tool")
        ));
        elided_tool_results.push(ElidedToolResult {
            tool_call_id: tool_use_id.clone(),
            tool_name,
            is_error: *is_error,
            canonical_content_kind,
            action: ToolResultElisionAction::Marker,
            canonical_content_bytes,
            projected_content_bytes: content.len(),
        });
    }

    let report = (!elided_tool_results.is_empty()).then(|| ToolResultProjectionReport {
        policy: RequestToolResultElisionPolicy::KeepRecent {
            configured_keep_recent_tool_results: keep_recent,
        },
        canonical_tool_result_content_bytes,
        projected_tool_result_content_bytes: tool_result_content_bytes(&compacted),
        results: elided_tool_results,
    });

    ProjectedToolResultHistory {
        messages: compacted,
        report,
    }
}

fn budget_tool_result_history(
    history: &[Message],
    budget: ProjectedToolResultBudget,
) -> ProjectedToolResultHistory {
    let mut projected = history.to_vec();
    let locations = tool_result_locations(history);
    let canonical_tool_result_content_bytes = locations.iter().fold(0usize, |total, location| {
        total.saturating_add(canonical_content(history, location).len())
    });
    if canonical_tool_result_content_bytes <= budget.max_bytes {
        return ProjectedToolResultHistory {
            messages: projected,
            report: None,
        };
    }

    let mut remaining = budget.max_bytes;
    let mut actions = vec![None; locations.len()];

    // Establish an honest floor for as many call/result pairs as the hard cap
    // permits before spending richer bytes on any one result.
    for index in (0..locations.len()).rev() {
        let location = &locations[index];
        let canonical = canonical_content(history, location);
        let marker = previous_tool_marker(location.tool_name.as_deref());
        let (content, action) = if canonical.len() <= marker.len() {
            (canonical.clone(), None)
        } else {
            (
                ToolResultContent::Text(marker),
                Some(ToolResultElisionAction::Marker),
            )
        };
        let (content, action) = if content.len() <= remaining {
            (content, action)
        } else if OMITTED_ELLIPSIS.len() <= remaining {
            (
                ToolResultContent::Text(OMITTED_ELLIPSIS.to_string()),
                Some(ToolResultElisionAction::Omitted),
            )
        } else {
            (
                ToolResultContent::Text(String::new()),
                Some(ToolResultElisionAction::Omitted),
            )
        };
        remaining -= content.len();
        set_projected_content(&mut projected, location, content);
        actions[index] = action;
    }

    let recent_start = locations
        .len()
        .saturating_sub(budget.prioritize_recent_results);

    // First priority: whole recent bodies.
    for index in (recent_start..locations.len()).rev() {
        try_restore_full(
            history,
            &mut projected,
            &locations[index],
            &mut actions[index],
            &mut remaining,
        );
    }

    // Second priority: bounded head/tail evidence across every changed text result.
    for index in (0..locations.len()).rev() {
        try_upgrade_preview(
            history,
            &mut projected,
            &locations[index],
            &mut actions[index],
            &mut remaining,
            budget.max_preview_bytes,
        );
    }

    // Last priority: whole historical bodies when every higher tier left room.
    for index in (0..recent_start).rev() {
        try_restore_full(
            history,
            &mut projected,
            &locations[index],
            &mut actions[index],
            &mut remaining,
        );
    }

    let projected_tool_result_content_bytes = tool_result_content_bytes(&projected);
    debug_assert!(projected_tool_result_content_bytes <= budget.max_bytes);
    let results = locations
        .iter()
        .zip(actions)
        .filter_map(|(location, action)| {
            action.map(|action| ElidedToolResult {
                tool_call_id: location.tool_use_id.clone(),
                tool_name: location.tool_name.clone(),
                is_error: location.is_error,
                canonical_content_kind: content_kind(canonical_content(history, location)),
                action,
                canonical_content_bytes: canonical_content(history, location).len(),
                projected_content_bytes: projected_content(&projected, location).len(),
            })
        })
        .collect::<Vec<_>>();

    ProjectedToolResultHistory {
        messages: projected,
        report: Some(ToolResultProjectionReport {
            policy: RequestToolResultElisionPolicy::ByteBudget {
                configured_max_bytes: budget.max_bytes,
                configured_prioritize_recent_results: budget.prioritize_recent_results,
                configured_max_preview_bytes: budget.max_preview_bytes,
            },
            canonical_tool_result_content_bytes,
            projected_tool_result_content_bytes,
            results,
        }),
    }
}

fn try_restore_full(
    canonical_history: &[Message],
    projected: &mut [Message],
    location: &ToolResultLocation,
    action: &mut Option<ToolResultElisionAction>,
    remaining: &mut usize,
) {
    if action.is_none() {
        return;
    }
    let canonical = canonical_content(canonical_history, location);
    let current_len = projected_content(projected, location).len();
    let Some(delta) = canonical.len().checked_sub(current_len) else {
        return;
    };
    if delta > *remaining {
        return;
    }
    set_projected_content(projected, location, canonical.clone());
    *remaining -= delta;
    *action = None;
}

fn try_upgrade_preview(
    canonical_history: &[Message],
    projected: &mut [Message],
    location: &ToolResultLocation,
    action: &mut Option<ToolResultElisionAction>,
    remaining: &mut usize,
    max_preview_bytes: usize,
) {
    if action.is_none() {
        return;
    }
    let ToolResultContent::Text(canonical) = canonical_content(canonical_history, location) else {
        return;
    };
    let current_len = projected_content(projected, location).len();
    let Some(allowance) = current_len.checked_add(*remaining) else {
        return;
    };
    let Some(preview) = text_preview(canonical, current_len, allowance, max_preview_bytes) else {
        return;
    };
    let delta = preview.len() - current_len;
    set_projected_content(projected, location, ToolResultContent::Text(preview));
    *remaining -= delta;
    *action = Some(ToolResultElisionAction::Preview);
}

fn text_preview(
    canonical: &str,
    current_len: usize,
    allowance: usize,
    max_preview_bytes: usize,
) -> Option<String> {
    let canonical_limit = canonical.len().checked_sub(1)?;
    let preview_limit = max_preview_bytes.min(allowance).min(canonical_limit);
    if preview_limit <= current_len || preview_limit <= PREVIEW_SEPARATOR.len() {
        return None;
    }

    let source_bytes = preview_limit - PREVIEW_SEPARATOR.len();
    let head_budget = source_bytes.div_ceil(2);
    let tail_budget = source_bytes / 2;
    let mut head_end = head_budget.min(canonical.len());
    while head_end > 0 && !canonical.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = canonical.len().saturating_sub(tail_budget);
    while tail_start < canonical.len() && !canonical.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    if head_end == 0
        || tail_start == canonical.len()
        || head_end >= tail_start
        || canonical[head_end..tail_start].chars().next().is_none()
    {
        return None;
    }

    let preview = format!(
        "{}{}{}",
        &canonical[..head_end],
        PREVIEW_SEPARATOR,
        &canonical[tail_start..]
    );
    (preview.len() > current_len
        && preview.len() <= preview_limit
        && preview.len() < canonical.len())
    .then_some(preview)
}

fn tool_result_locations(history: &[Message]) -> Vec<ToolResultLocation> {
    let tool_names = tool_name_index(history);
    let mut locations = Vec::new();
    for (message_index, message) in history.iter().enumerate() {
        for (block_index, block) in message.content.iter().enumerate() {
            if let ContentBlock::ToolResult {
                tool_use_id,
                is_error,
                ..
            } = block
            {
                locations.push(ToolResultLocation {
                    message_index,
                    block_index,
                    tool_use_id: tool_use_id.clone(),
                    tool_name: tool_names.get(tool_use_id).cloned(),
                    is_error: *is_error,
                });
            }
        }
    }
    locations
}

fn projected_content<'a>(
    messages: &'a [Message],
    location: &ToolResultLocation,
) -> &'a ToolResultContent {
    let ContentBlock::ToolResult { content, .. } =
        &messages[location.message_index].content[location.block_index]
    else {
        unreachable!("a collected tool-result location remains a tool result")
    };
    content
}

fn canonical_content<'a>(
    messages: &'a [Message],
    location: &ToolResultLocation,
) -> &'a ToolResultContent {
    projected_content(messages, location)
}

fn set_projected_content(
    messages: &mut [Message],
    location: &ToolResultLocation,
    projected: ToolResultContent,
) {
    let ContentBlock::ToolResult { content, .. } =
        &mut messages[location.message_index].content[location.block_index]
    else {
        unreachable!("a collected tool-result location remains a tool result")
    };
    *content = projected;
}

fn previous_tool_marker(tool_name: Option<&str>) -> String {
    format!("[Previous: used {}]", tool_name.unwrap_or("tool"))
}

fn content_kind(content: &ToolResultContent) -> ToolResultContentKind {
    match content {
        ToolResultContent::Text(_) => ToolResultContentKind::Text,
        ToolResultContent::Structured(_) => ToolResultContentKind::Structured,
    }
}

fn tool_result_content_bytes(history: &[Message]) -> usize {
    history.iter().fold(0usize, |total, message| {
        message.content.iter().fold(total, |total, block| {
            let bytes = match block {
                ContentBlock::ToolResult { content, .. } => content.len(),
                _ => 0,
            };
            total.saturating_add(bytes)
        })
    })
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
        assert!(projected.report.is_none());
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
            projected.report.as_ref().unwrap().results,
            vec![ElidedToolResult {
                tool_call_id: "long-old".to_string(),
                tool_name: Some("read_file".to_string()),
                is_error: true,
                canonical_content_kind: ToolResultContentKind::Text,
                action: ToolResultElisionAction::Marker,
                canonical_content_bytes: 101,
                projected_content_bytes: "[Previous: used read_file]".len(),
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

    fn text_result(id: &str, name: &str, text: impl Into<String>) -> Vec<Message> {
        vec![
            Message::assistant(ContentBlock::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input: serde_json::json!({}),
            }),
            Message::user(ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: ToolResultContent::text(text),
                is_error: false,
            }),
        ]
    }

    fn structured_result(id: &str, name: &str, value: serde_json::Value) -> Vec<Message> {
        vec![
            Message::assistant(ContentBlock::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input: serde_json::json!({}),
            }),
            Message::user(ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: ToolResultContent::Structured(value),
                is_error: false,
            }),
        ]
    }

    fn result_contents(history: &[Message]) -> Vec<ToolResultContent> {
        history
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect()
    }

    fn budget(max_bytes: usize, recent: usize, preview: usize) -> ProjectedToolResultBudget {
        ProjectedToolResultBudget {
            max_bytes,
            prioritize_recent_results: recent,
            max_preview_bytes: preview,
        }
    }

    #[test]
    fn byte_budget_wins_over_finite_legacy_policy_and_is_identity_at_the_exact_cap() {
        let mut history = text_result("one", "read", "1234567890");
        history.extend(text_result("two", "read", "abcdefghij"));

        let projected = project_tool_result_history(&history, 0, Some(budget(20, 0, 0)));

        assert_eq!(projected.messages, history);
        assert!(projected.report.is_none());
    }

    #[test]
    fn budgets_below_one_ellipsis_empty_the_body_without_breaking_pairing() {
        let history = vec![Message::user(ContentBlock::ToolResult {
            tool_use_id: "call".to_string(),
            content: ToolResultContent::text("long body"),
            is_error: true,
        })];
        let canonical = history.clone();

        for (max_bytes, expected) in [(0, ""), (1, ""), (2, ""), (3, "…")] {
            let projected = budget_tool_result_history(&history, budget(max_bytes, 0, 0));
            let ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } = &projected.messages[0].content[0]
            else {
                panic!("tool-result pairing must survive")
            };
            assert_eq!(tool_use_id, "call");
            assert!(*is_error);
            assert_eq!(content, expected);
            assert!(tool_result_content_bytes(&projected.messages) <= max_bytes);
            let detail = &projected.report.as_ref().unwrap().results[0];
            assert_eq!(detail.action, ToolResultElisionAction::Omitted);
            assert_eq!(detail.projected_content_bytes, expected.len());
        }
        assert_eq!(history, canonical);
    }

    #[test]
    fn marker_floors_are_shared_before_the_newest_result_is_upgraded() {
        let mut history = text_result("old", "read", "o".repeat(100));
        history.extend(text_result("new", "read", "n".repeat(100)));
        let marker = previous_tool_marker(Some("read"));
        let max_bytes = marker.len() + OMITTED_ELLIPSIS.len();

        let projected = budget_tool_result_history(&history, budget(max_bytes, 1, 100));
        let contents = result_contents(&projected.messages);

        assert_eq!(contents[0], OMITTED_ELLIPSIS);
        assert_eq!(contents[1], marker.as_str());
        assert_eq!(tool_result_content_bytes(&projected.messages), max_bytes);
        assert_eq!(
            projected
                .report
                .as_ref()
                .unwrap()
                .results
                .iter()
                .map(|result| (result.tool_call_id.as_str(), result.action))
                .collect::<Vec<_>>(),
            vec![
                ("old", ToolResultElisionAction::Omitted),
                ("new", ToolResultElisionAction::Marker),
            ]
        );
    }

    #[test]
    fn descriptive_marker_uses_ellipsis_when_it_is_one_byte_too_large() {
        let history = text_result("call", "read", "x".repeat(100));
        let marker = previous_tool_marker(Some("read"));

        let exact = budget_tool_result_history(&history, budget(marker.len(), 0, 0));
        assert_eq!(result_contents(&exact.messages)[0], marker.as_str());
        assert_eq!(
            exact.report.as_ref().unwrap().results[0].action,
            ToolResultElisionAction::Marker
        );

        let short = budget_tool_result_history(&history, budget(marker.len() - 1, 0, 0));
        assert_eq!(result_contents(&short.messages)[0], OMITTED_ELLIPSIS);
        assert_eq!(
            short.report.as_ref().unwrap().results[0].action,
            ToolResultElisionAction::Omitted
        );
    }

    #[test]
    fn recent_whole_body_precedes_historical_preview_and_full_upgrades() {
        let mut history = text_result("old", "read", "o".repeat(100));
        history.extend(text_result("new", "read", "n".repeat(100)));
        let marker_len = previous_tool_marker(Some("read")).len();
        let max_bytes = marker_len + 100;

        let projected = budget_tool_result_history(&history, budget(max_bytes, 1, 80));
        let contents = result_contents(&projected.messages);

        assert_eq!(contents[0], previous_tool_marker(Some("read")).as_str());
        assert_eq!(contents[1], "n".repeat(100).as_str());
        assert_eq!(tool_result_content_bytes(&projected.messages), max_bytes);
    }

    #[test]
    fn text_preview_is_bounded_including_its_separator_and_invents_no_reader() {
        let mut history = text_result("old", "read", "o".repeat(100));
        history.extend(text_result(
            "new",
            "read",
            "abcdefghijklmnopqrstuvwxyz".repeat(4),
        ));
        let canonical = history.clone();
        let marker_len = previous_tool_marker(Some("read")).len();
        let preview_bytes = 40;

        let projected = budget_tool_result_history(
            &history,
            budget(marker_len + preview_bytes, 0, preview_bytes),
        );
        let contents = result_contents(&projected.messages);
        let preview = contents[1].as_str();

        assert_eq!(contents[0], previous_tool_marker(Some("read")).as_str());
        assert_eq!(preview.len(), preview_bytes);
        assert!(preview.contains(PREVIEW_SEPARATOR));
        assert!(!preview.contains("read_tool_result"));
        assert!(!preview.contains("tool_use_id="));
        assert_eq!(
            projected.report.as_ref().unwrap().results[1].action,
            ToolResultElisionAction::Preview
        );
        assert_eq!(history, canonical);
    }

    #[test]
    fn utf8_preview_uses_the_fixed_split_without_rebalancing_rounding_slack() {
        let canonical = format!("😀{}界", "a".repeat(40));
        let preview = text_preview(&canonical, 3, 24, 24).expect("preview fits");

        assert_eq!(preview, format!("😀{PREVIEW_SEPARATOR}界"));
        assert_eq!(preview.len(), 24);
        assert!(preview.is_char_boundary(preview.len()));
        assert!(text_preview("😀界", 0, 100, 100).is_none());
        assert!(text_preview(&canonical, 3, 23, 23).is_none());
    }

    #[test]
    fn budget_counts_all_roles_while_legacy_keeps_ignoring_non_user_results() {
        let structured = ToolResultContent::Structured(serde_json::json!({
            "evidence": "x".repeat(80)
        }));
        let history = vec![Message::assistant(ContentBlock::ToolResult {
            tool_use_id: "structured".to_string(),
            content: structured.clone(),
            is_error: false,
        })];

        let legacy = micro_compact_history(&history, 0);
        assert_eq!(legacy.messages, history);
        assert!(legacy.report.is_none());

        let projected = budget_tool_result_history(&history, budget(5, 0, 100));
        let contents = result_contents(&projected.messages);
        assert_eq!(contents, vec![ToolResultContent::text(OMITTED_ELLIPSIS)]);
        let detail = &projected.report.as_ref().unwrap().results[0];
        assert_eq!(
            detail.canonical_content_kind,
            ToolResultContentKind::Structured
        );
        assert_eq!(detail.action, ToolResultElisionAction::Omitted);
        assert_eq!(
            history[0].content[0],
            ContentBlock::ToolResult {
                tool_use_id: "structured".to_string(),
                content: structured,
                is_error: false,
            }
        );
    }

    #[test]
    fn budget_projection_is_deterministic_and_reports_exact_chronological_totals() {
        let mut history = text_result("one", "a", "1".repeat(60));
        history.extend(text_result("two", "b", "2".repeat(70)));
        history.extend(text_result("three", "c", "3".repeat(80)));
        let config = budget(75, 1, 35);

        let first = budget_tool_result_history(&history, config);
        let second = budget_tool_result_history(&history, config);

        assert_eq!(first, second);
        let report = first.report.as_ref().unwrap();
        assert_eq!(report.canonical_tool_result_content_bytes, 210);
        assert_eq!(
            report.projected_tool_result_content_bytes,
            tool_result_content_bytes(&first.messages)
        );
        assert!(report.projected_tool_result_content_bytes <= config.max_bytes);
        assert_eq!(
            report
                .results
                .iter()
                .map(|result| result.tool_call_id.as_str())
                .collect::<Vec<_>>(),
            vec!["one", "two", "three"]
        );
    }

    #[test]
    fn structured_results_are_atomic_markers_or_exact_whole_values() {
        let old_value = serde_json::json!({ "old": "o".repeat(80) });
        let new_value = serde_json::json!({ "new": "n".repeat(80) });
        let mut history = structured_result("old", "query", old_value.clone());
        history.extend(structured_result("new", "query", new_value.clone()));
        let marker = previous_tool_marker(Some("query"));
        let new_len = ToolResultContent::Structured(new_value.clone()).len();

        let projected =
            budget_tool_result_history(&history, budget(marker.len() + new_len, 1, usize::MAX));
        let contents = result_contents(&projected.messages);

        assert_eq!(contents[0], marker.as_str());
        assert_eq!(contents[1], ToolResultContent::Structured(new_value));
        assert_eq!(
            projected.report.as_ref().unwrap().results[0].action,
            ToolResultElisionAction::Marker
        );
        assert_eq!(
            history[1].content[0],
            ContentBlock::ToolResult {
                tool_use_id: "old".to_string(),
                content: ToolResultContent::Structured(old_value),
                is_error: false,
            }
        );
    }

    #[test]
    fn previews_are_broader_priority_than_restoring_one_large_historical_body() {
        let mut history = text_result("old", "read", "o".repeat(100));
        history.extend(text_result("new", "read", "n".repeat(100)));
        let marker_len = previous_tool_marker(Some("read")).len();

        let projected = budget_tool_result_history(&history, budget(marker_len + 100, 0, 60));
        let contents = result_contents(&projected.messages);

        assert_eq!(contents[0].len(), 60);
        assert_eq!(contents[1].len(), 60);
        assert!(
            contents
                .iter()
                .all(|content| content.contains(PREVIEW_SEPARATOR))
        );
    }

    #[test]
    fn zero_preview_cap_still_allows_the_historical_full_tier() {
        let mut history = text_result("old", "read", "o".repeat(100));
        history.extend(text_result("new", "read", "n".repeat(100)));
        let marker_len = previous_tool_marker(Some("read")).len();

        let projected = budget_tool_result_history(&history, budget(marker_len + 100, 0, 0));
        let contents = result_contents(&projected.messages);

        assert_eq!(contents[0], previous_tool_marker(Some("read")).as_str());
        assert_eq!(contents[1], "n".repeat(100).as_str());
        assert!(
            contents
                .iter()
                .all(|content| !content.contains(PREVIEW_SEPARATOR))
        );
    }

    #[test]
    fn historical_full_tier_skips_an_unaffordable_body_and_restores_the_next_one() {
        let mut history = text_result("tiny", "read", "tiny");
        history.extend(text_result("medium", "read", "m".repeat(30)));
        history.extend(text_result("large", "read", "l".repeat(100)));
        let marker_len = previous_tool_marker(Some("read")).len();
        let max_bytes = "tiny".len() + 2 * marker_len + (30 - marker_len);

        let projected = budget_tool_result_history(&history, budget(max_bytes, 0, 0));
        let contents = result_contents(&projected.messages);

        assert_eq!(contents[0], "tiny");
        assert_eq!(contents[1], "m".repeat(30).as_str());
        assert_eq!(contents[2], previous_tool_marker(Some("read")).as_str());
        assert_eq!(
            projected
                .report
                .as_ref()
                .unwrap()
                .results
                .iter()
                .map(|result| result.tool_call_id.as_str())
                .collect::<Vec<_>>(),
            vec!["large"]
        );
    }

    #[test]
    fn strict_cap_holds_across_mixed_content_and_every_small_budget() {
        let mut history = text_result("ascii", "read", "a".repeat(31));
        history.extend(text_result("unicode", "read", "😀界".repeat(9)));
        history.extend(structured_result(
            "json",
            "query",
            serde_json::json!({ "items": [1, 2, 3], "text": "z".repeat(23) }),
        ));
        history.push(Message::assistant(ContentBlock::ToolResult {
            tool_use_id: "assistant-result".to_string(),
            content: ToolResultContent::text("assistant role"),
            is_error: true,
        }));
        history.push(Message::user(ContentBlock::ToolResult {
            tool_use_id: "empty".to_string(),
            content: ToolResultContent::text(""),
            is_error: false,
        }));
        let canonical = history.clone();
        let total = tool_result_content_bytes(&history);
        let result_count = tool_result_locations(&history).len();

        for max_bytes in 0..=total {
            for recent in [0, 1, result_count, result_count + 1] {
                for preview in [0, 16, 17, 18, total + 1] {
                    let config = budget(max_bytes, recent, preview);
                    let first = budget_tool_result_history(&history, config);
                    let second = budget_tool_result_history(&history, config);
                    assert_eq!(first, second);
                    assert_eq!(history, canonical);
                    assert!(tool_result_content_bytes(&first.messages) <= max_bytes);
                    assert_eq!(first.messages.len(), history.len());
                    for (projected, canonical) in first.messages.iter().zip(&history) {
                        assert_eq!(projected.role, canonical.role);
                        assert_eq!(projected.content.len(), canonical.content.len());
                        for (projected, canonical) in
                            projected.content.iter().zip(&canonical.content)
                        {
                            match (projected, canonical) {
                                (
                                    ContentBlock::ToolResult {
                                        tool_use_id: projected_id,
                                        is_error: projected_error,
                                        ..
                                    },
                                    ContentBlock::ToolResult {
                                        tool_use_id: canonical_id,
                                        is_error: canonical_error,
                                        ..
                                    },
                                ) => {
                                    assert_eq!(projected_id, canonical_id);
                                    assert_eq!(projected_error, canonical_error);
                                }
                                _ => assert_eq!(projected, canonical),
                            }
                        }
                    }
                }
            }
        }
    }
}
