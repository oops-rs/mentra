use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ContentBlock, Message, Role};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentTranscript {
    items: Vec<TranscriptItem>,
}

impl AgentTranscript {
    pub fn new(items: Vec<TranscriptItem>) -> Self {
        Self { items }
    }

    pub fn from_messages(messages: Vec<Message>) -> Self {
        Self {
            items: messages
                .into_iter()
                .map(transcript_item_from_message)
                .collect(),
        }
    }

    pub fn items(&self) -> &[TranscriptItem] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn push(&mut self, item: TranscriptItem) {
        self.items.push(item);
    }

    pub fn to_messages(&self) -> Vec<Message> {
        self.items
            .iter()
            .filter_map(TranscriptItem::project_message)
            .collect()
    }

    pub fn projected_messages_from(&self, start: usize) -> Vec<Message> {
        self.items
            .iter()
            .skip(start)
            .filter_map(TranscriptItem::project_message)
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptItem {
    pub kind: TranscriptKind,
    pub message: Option<Message>,
    /// Opaque per-call host metadata attached via [`TranscriptItem::with_details`]
    /// (populated from [`crate::tool::ToolOutput::details`]), keyed by
    /// `tool_use_id` because one tool-result message can carry several
    /// results. mentra never interprets these values; they survive
    /// transcript persistence and replay but are never projected into a
    /// provider request — [`TranscriptItem::project_message`] only ever
    /// returns `message`. `serde(default)` keeps transcripts persisted
    /// before this field existed deserializing unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    details: Option<BTreeMap<String, Value>>,
}

impl TranscriptItem {
    pub fn user_turn(message: Message) -> Self {
        Self {
            kind: TranscriptKind::UserTurn,
            message: Some(message),
            details: None,
        }
    }

    pub fn assistant_turn(message: Message) -> Self {
        Self {
            kind: TranscriptKind::AssistantTurn,
            message: Some(message),
            details: None,
        }
    }

    pub fn tool_exchange(message: Message, tool_use_id: Option<String>, is_error: bool) -> Self {
        Self {
            kind: TranscriptKind::ToolExchange {
                tool_use_id,
                is_error,
            },
            message: Some(message),
            details: None,
        }
    }

    pub fn canonical_context(message: Message) -> Self {
        Self {
            kind: TranscriptKind::CanonicalContext,
            message: Some(message),
            details: None,
        }
    }

    pub fn delegation_request(
        message: Message,
        delegation: DelegationArtifact,
        edge: Option<DelegationEdge>,
    ) -> Self {
        Self {
            kind: TranscriptKind::DelegationRequest { delegation, edge },
            message: Some(message),
            details: None,
        }
    }

    pub fn delegation_result(
        message: Message,
        delegation: DelegationArtifact,
        edge: Option<DelegationEdge>,
    ) -> Self {
        Self {
            kind: TranscriptKind::DelegationResult { delegation, edge },
            message: Some(message),
            details: None,
        }
    }

    pub fn compaction_summary(summary: CompactionSummary) -> Self {
        Self {
            message: Some(Message::user(ContentBlock::text(
                summary.render_for_handoff(),
            ))),
            kind: TranscriptKind::CompactionSummary { summary },
            details: None,
        }
    }

    /// Attaches opaque per-call host metadata to this item, keyed by
    /// `tool_use_id`. A no-op for an empty map, so attaching a possibly-empty
    /// collected map never turns a details-free item into one carrying
    /// `Some(empty map)`.
    pub fn with_details(mut self, details: BTreeMap<String, Value>) -> Self {
        if !details.is_empty() {
            self.details = Some(details);
        }
        self
    }

    /// This item's opaque per-call host metadata, if any. mentra never
    /// interprets these values — a host recovers its own metadata after a
    /// round through this accessor alone, without mentra knowing any host
    /// type.
    pub fn details(&self) -> Option<&BTreeMap<String, Value>> {
        self.details.as_ref()
    }

    /// Looks up this item's opaque metadata for one `tool_use_id`.
    pub fn detail(&self, tool_use_id: &str) -> Option<&Value> {
        self.details.as_ref()?.get(tool_use_id)
    }

    pub fn project_message(&self) -> Option<Message> {
        self.message.clone()
    }

    pub fn is_real_user_turn(&self) -> bool {
        matches!(self.kind, TranscriptKind::UserTurn)
    }

    pub fn is_delegation_result(&self) -> bool {
        matches!(self.kind, TranscriptKind::DelegationResult { .. })
    }

    pub fn text(&self) -> String {
        self.message.as_ref().map(Message::text).unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptKind {
    UserTurn,
    AssistantTurn,
    ToolExchange {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        is_error: bool,
    },
    CanonicalContext,
    MemoryRecall,
    DelegationRequest {
        delegation: DelegationArtifact,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        edge: Option<DelegationEdge>,
    },
    DelegationResult {
        delegation: DelegationArtifact,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        edge: Option<DelegationEdge>,
    },
    CompactionSummary {
        summary: CompactionSummary,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationKind {
    Subagent,
    Teammate,
    Parent,
    Child,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationStatus {
    Requested,
    Finished,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationEdge {
    pub kind: DelegationKind,
    pub local_agent_id: String,
    pub remote_agent_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationArtifact {
    pub kind: DelegationKind,
    pub agent_id: String,
    pub agent_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub status: DelegationStatus,
    pub task_summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<String>,
    #[serde(default)]
    pub artifacts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CompactionSummary {
    pub goal: String,
    pub progress: String,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub delegated_work: Vec<String>,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub open_questions: Vec<String>,
    #[serde(default)]
    pub next_steps: Vec<String>,
}

impl CompactionSummary {
    pub fn render_for_handoff(&self) -> String {
        let mut lines = vec![
            "[Compaction summary]".to_string(),
            format!("Goal: {}", fallback_text(&self.goal)),
            format!("Progress: {}", fallback_text(&self.progress)),
        ];
        append_list(&mut lines, "Decisions", &self.decisions);
        append_list(&mut lines, "Constraints", &self.constraints);
        append_list(&mut lines, "Delegated work", &self.delegated_work);
        append_list(&mut lines, "Artifacts", &self.artifacts);
        append_list(&mut lines, "Open questions", &self.open_questions);
        append_list(&mut lines, "Next steps", &self.next_steps);
        lines.join("\n")
    }

    pub fn from_fallback_text(text: String) -> Self {
        Self {
            progress: text,
            next_steps: vec![
                "Review the preserved transcript tail and continue from there.".to_string(),
            ],
            ..Self::default()
        }
    }
}

pub(crate) fn transcript_item_from_message(message: Message) -> TranscriptItem {
    match message.role {
        Role::Assistant => TranscriptItem::assistant_turn(message),
        Role::User => {
            if let Some((tool_use_id, is_error)) =
                message.content.first().and_then(|block| match block {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        is_error,
                        ..
                    } => Some((tool_use_id.clone(), *is_error)),
                    _ => None,
                })
            {
                TranscriptItem::tool_exchange(message, Some(tool_use_id), is_error)
            } else {
                TranscriptItem::user_turn(message)
            }
        }
        Role::Unknown(_) => TranscriptItem::user_turn(message),
    }
}

fn append_list(lines: &mut Vec<String>, label: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    lines.push(format!("{label}:"));
    for item in items {
        lines.push(format!("- {item}"));
    }
}

fn fallback_text(text: &str) -> &str {
    if text.trim().is_empty() {
        "(none)"
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Old-format compatibility (M3 test 6): a transcript persisted before
    // `details` existed is exactly the JSON a details-free item serializes
    // to today (the field is `skip_serializing_if` on `None`), so proving
    // that JSON deserializes back to `details: None` proves genuinely old
    // persisted transcripts still load.
    #[test]
    fn item_without_details_serializes_and_deserializes_as_old_format() {
        let item = TranscriptItem::user_turn(Message::user(ContentBlock::text("hello")));
        let json = serde_json::to_string(&item).expect("serialize");
        assert!(
            !json.contains("details"),
            "a details-free item must serialize identically to pre-M3 transcripts, got: {json}"
        );

        let reloaded: TranscriptItem = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(reloaded.details(), None);
        assert_eq!(reloaded, item);
    }

    #[test]
    fn details_round_trip_through_json_keyed_by_tool_use_id() {
        let mut details = BTreeMap::new();
        details.insert("call-1".to_string(), json!({ "secret": "shh" }));
        let item = TranscriptItem::tool_exchange(
            Message::user(ContentBlock::text("result")),
            Some("call-1".to_string()),
            false,
        )
        .with_details(details.clone());

        let json = serde_json::to_string(&item).expect("serialize");
        let reloaded: TranscriptItem = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(reloaded.details(), Some(&details));
        assert_eq!(reloaded.detail("call-1"), Some(&json!({ "secret": "shh" })));
        assert_eq!(reloaded.detail("call-2"), None);
    }

    #[test]
    fn with_details_is_a_no_op_for_an_empty_map() {
        let item = TranscriptItem::user_turn(Message::user(ContentBlock::text("hello")))
            .with_details(BTreeMap::new());
        assert_eq!(item.details(), None);
    }
}
