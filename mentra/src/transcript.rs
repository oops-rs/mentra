use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{ContentBlock, Message, Role};

static NEXT_ENTRY_SUFFIX: AtomicU64 = AtomicU64::new(0);

/// Identifier for one transcript entry.
///
/// Entries form a tree through [`TranscriptItem::parent_id`]; this is how a
/// conversation can return to an earlier point and continue along a different
/// path without copying history.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EntryId(String);

impl EntryId {
    pub fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let suffix = NEXT_ENTRY_SUFFIX.fetch_add(1, Ordering::Relaxed);
        Self(format!("entry-{stamp:x}-{suffix:x}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for EntryId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for EntryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a branch operation could not be performed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BranchError {
    #[error("no entry '{0}' on the current path")]
    UnknownEntry(EntryId),
}

/// An agent's conversation, as a tree of entries with one active path.
///
/// [`items`](Self::items) is that active path, root to leaf — the messages
/// the model actually sees, and the only view most code needs. Entries left
/// behind by [`branch_from`](Self::branch_from) move to
/// [`archived`](Self::archived) rather than being deleted, so a branch is a
/// move of the leaf pointer rather than a copy of history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(from = "AgentTranscriptWire")]
pub struct AgentTranscript {
    items: Vec<TranscriptItem>,
    /// Entries off the active path. Reachable through
    /// [`children`](Self::children) so an abandoned branch can be inspected
    /// or returned to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    archive: Vec<TranscriptItem>,
}

/// Deserialization shape, so transcripts written before entries had ids load
/// unchanged and get their parent links filled in on the way through.
#[derive(Deserialize)]
struct AgentTranscriptWire {
    #[serde(default)]
    items: Vec<TranscriptItem>,
    #[serde(default)]
    archive: Vec<TranscriptItem>,
}

impl From<AgentTranscriptWire> for AgentTranscript {
    fn from(wire: AgentTranscriptWire) -> Self {
        let mut transcript = Self {
            items: wire.items,
            archive: wire.archive,
        };
        transcript.link_active_path();
        transcript
    }
}

impl AgentTranscript {
    pub fn new(items: Vec<TranscriptItem>) -> Self {
        let mut transcript = Self {
            items,
            archive: Vec::new(),
        };
        transcript.link_active_path();
        transcript
    }

    pub fn from_messages(messages: Vec<Message>) -> Self {
        Self::new(
            messages
                .into_iter()
                .map(transcript_item_from_message)
                .collect(),
        )
    }

    /// Fills in parent links the active path implies.
    ///
    /// The active path is a root-to-leaf chain by construction, so an entry's
    /// parent is the entry before it. Only missing links are written, which
    /// leaves a tree loaded from disk alone and repairs a transcript written
    /// before entries had ids.
    fn link_active_path(&mut self) {
        for index in 1..self.items.len() {
            if self.items[index].parent_id.is_none() {
                self.items[index].parent_id = Some(self.items[index - 1].id.clone());
            }
        }
    }

    /// The entry the next append will hang from.
    pub fn leaf(&self) -> Option<&EntryId> {
        self.items.last().map(|item| &item.id)
    }

    /// Entries that are not on the active path.
    pub fn archived(&self) -> &[TranscriptItem] {
        &self.archive
    }

    /// Looks up an entry anywhere in the tree.
    pub fn entry(&self, id: &EntryId) -> Option<&TranscriptItem> {
        self.items
            .iter()
            .chain(self.archive.iter())
            .find(|item| &item.id == id)
    }

    /// The entries recorded as continuing from `id`, in creation order.
    ///
    /// More than one means the conversation branched there: each is the start
    /// of a different path explored from the same point.
    pub fn children(&self, id: &EntryId) -> Vec<&TranscriptItem> {
        self.items
            .iter()
            .chain(self.archive.iter())
            .filter(|item| item.parent_id.as_ref() == Some(id))
            .collect()
    }

    /// Moves the leaf back to `id`, so subsequent appends continue from there.
    ///
    /// Entries after `id` are moved off the active path, not deleted: they
    /// stay reachable through [`children`](Self::children), which is what
    /// makes this a branch rather than a truncation. Returns how many entries
    /// left the path.
    pub fn branch_from(&mut self, id: &EntryId) -> Result<usize, BranchError> {
        let Some(position) = self.items.iter().position(|item| &item.id == id) else {
            return Err(BranchError::UnknownEntry(id.clone()));
        };

        let abandoned = self.items.split_off(position + 1);
        let count = abandoned.len();
        self.archive.extend(abandoned);
        Ok(count)
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

    /// Appends an entry as a child of the current leaf.
    pub fn push(&mut self, mut item: TranscriptItem) {
        item.parent_id = self.leaf().cloned();
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
    /// Identity of this entry within the transcript tree.
    #[serde(default)]
    pub id: EntryId,
    /// The entry this one continues from. `None` marks a root.
    ///
    /// Set by [`AgentTranscript::push`] rather than by the constructors: an
    /// entry's parent is a property of where it is appended, not of what it
    /// contains.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<EntryId>,
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
            id: EntryId::new(),
            parent_id: None,
            kind: TranscriptKind::UserTurn,
            message: Some(message),
            details: None,
        }
    }

    pub fn assistant_turn(message: Message) -> Self {
        Self {
            id: EntryId::new(),
            parent_id: None,
            kind: TranscriptKind::AssistantTurn,
            message: Some(message),
            details: None,
        }
    }

    pub fn tool_exchange(message: Message, tool_use_id: Option<String>, is_error: bool) -> Self {
        Self {
            id: EntryId::new(),
            parent_id: None,
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
            id: EntryId::new(),
            parent_id: None,
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
            id: EntryId::new(),
            parent_id: None,
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
            id: EntryId::new(),
            parent_id: None,
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
            id: EntryId::new(),
            parent_id: None,
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
    /// Files the agent has read or modified, accumulated across every
    /// compaction in this transcript's history.
    ///
    /// Carried structurally rather than left to the model's prose: a summary
    /// is itself summarized by the next compaction, so a file list that lived
    /// only in `progress` decayed out of context after two or three rounds,
    /// silently. The agent would simply stop knowing it edited something an
    /// hour ago.
    #[serde(default)]
    pub files_touched: Vec<String>,
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
        append_list(&mut lines, "Files touched", &self.files_touched);
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

    // M3 test 2 (projection-boundary half): `to_messages()`/`project_message`
    // are the single place internal transcript state turns into what a
    // provider request carries (`Message`/`ContentBlock`) — proving details
    // never appears in that projection, independent of any live agent
    // plumbing, is what makes "provider requests receive only content" true
    // by construction rather than by convention. The live round-trip through
    // a real model request is covered by
    // `agent::tests::tool_output::structured_tool_projects_content_and_hides_details_from_provider`.
    #[test]
    fn to_messages_projection_never_carries_details() {
        let mut details = BTreeMap::new();
        details.insert("call-1".to_string(), json!({ "secret": "shh" }));
        let transcript = AgentTranscript::new(vec![
            TranscriptItem::user_turn(Message::user(ContentBlock::text("go"))),
            TranscriptItem::assistant_turn(Message::assistant(ContentBlock::ToolUse {
                id: "call-1".to_string(),
                name: "structured_details_tool".to_string(),
                input: json!({}),
            })),
            TranscriptItem::tool_exchange(
                Message::user(ContentBlock::ToolResult {
                    tool_use_id: "call-1".to_string(),
                    content: crate::tool::ToolResultContent::Structured(json!({ "answer": 42 })),
                    is_error: false,
                }),
                Some("call-1".to_string()),
                false,
            )
            .with_details(details),
        ]);

        let projected = serde_json::to_string(&transcript.to_messages()).expect("serialize");
        assert!(projected.contains("answer"), "content must still project");
        assert!(!projected.contains("secret"));
        assert!(!projected.contains("shh"));
    }
}
