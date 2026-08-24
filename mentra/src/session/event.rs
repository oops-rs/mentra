use serde::{Deserialize, Serialize};

use super::types::SessionId;
use crate::tool::ToolClassification;

pub type EventSeq = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolMutability {
    ReadOnly,
    Mutating,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskLifecycleStatus {
    Spawned,
    Running,
    Finished,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Subagent,
    BackgroundTask,
    Teammate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOutcome {
    Allowed,
    Denied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionRuleScope {
    Session,
    Project,
    Global,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeSeverity {
    Info,
    Warning,
}

/// Events emitted during a session lifecycle.
///
/// The enum derives `Eq`, so every field of every variant has to. That is not
/// what keeps [`PermissionRequested::preview`](Self::PermissionRequested) a
/// `String`: `serde_json` does implement `Eq` for `Value`, a `Value` never
/// holding a non-finite float. It stays a `String` because that exact text is
/// what a remembered rule's
/// [`pattern`](crate::RuleKey::pattern) is matched against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    SessionStarted {
        session_id: SessionId,
    },
    UserMessage {
        text: String,
        /// How many images the turn carried.
        ///
        /// A turn can be images alone — a screenshot pasted with nothing typed
        /// — and `text` is then empty. A client rendering only `text` shows a
        /// blank user message and the transcript looks like the person sent
        /// nothing, so the count is here to be rendered as the attachment it
        /// is.
        #[serde(default, skip_serializing_if = "is_zero_usize")]
        image_count: usize,
    },
    AssistantTokenDelta {
        delta: String,
        full_text: String,
    },
    AssistantReasoningDelta {
        delta: String,
        full_text: String,
    },
    AssistantMessageCompleted {
        text: String,
    },
    ToolQueued {
        tool_call_id: String,
        tool_name: String,
        summary: String,
        /// Always [`ToolMutability::Unknown`] today.
        ///
        /// A queued call is announced while the provider stream is still being
        /// decoded, from a content block that carries an id, a name and input
        /// JSON and nothing about the tool behind them. A host that needs to
        /// know what a call would do reads
        /// [`PermissionRequested::classification`](Self::PermissionRequested),
        /// which is both typed and finer: it separates a local write from a
        /// process launch from a network call, where this field's three values
        /// cannot.
        mutability: ToolMutability,
        input_json: String,
    },
    ToolStarted {
        tool_call_id: String,
        tool_name: String,
    },
    ToolProgress {
        tool_call_id: String,
        tool_name: String,
        progress: String,
    },
    ToolCompleted {
        tool_call_id: String,
        tool_name: String,
        summary: String,
        is_error: bool,
    },
    PermissionRequested {
        request_id: String,
        tool_call_id: String,
        tool_name: String,
        description: String,
        /// The call's structured input, JSON-encoded.
        ///
        /// This is byte-for-byte the text a
        /// [`RuleKey::pattern`](crate::RuleKey::pattern) is matched against, so
        /// a host can write a remembered rule straight from what it showed the
        /// user and know the rule will answer this call.
        ///
        /// It describes the call's arguments and nothing else — what the call
        /// is allowed to do is `classification`, which is typed for the
        /// purpose and needs no parsing.
        preview: String,
        /// What the call was classified as before the approver was asked.
        ///
        /// A host subscribed to this stream can write a policy against what a
        /// call *does* rather than against its name: "allow edits, refuse the
        /// network" is [`ToolSideEffectLevel::LocalState`] against
        /// [`ToolSideEffectLevel::External`], readable from this field alone.
        ///
        /// It describes the call **as it stood when the approver was asked**.
        /// A [`PreExecutionHook`] returning
        /// [`HookDecision::Modify`](crate::runtime::HookDecision::Modify) runs
        /// afterwards and can replace the input wholesale, and for a tool whose
        /// classification depends on its input the executed call may then
        /// classify differently. A host writing policy here and rewriting
        /// inputs there is defeating its own policy, but nothing stops it.
        ///
        /// [`None`] only on an event deserialized from a stream recorded
        /// before this field existed; every request a session emits carries
        /// [`Some`]. It is an [`Option`] rather than a defaulted
        /// [`ToolClassification`] because an absent classification is unknown,
        /// not harmless — a default would let a replayed event read as a call
        /// that touches nothing, and a policy has to decide for itself what to
        /// do with one it cannot classify.
        ///
        /// [`ToolSideEffectLevel::LocalState`]: crate::tool::ToolSideEffectLevel::LocalState
        /// [`ToolSideEffectLevel::External`]: crate::tool::ToolSideEffectLevel::External
        /// [`PreExecutionHook`]: crate::runtime::PreExecutionHook
        #[serde(default)]
        classification: Option<ToolClassification>,
    },
    PermissionResolved {
        request_id: String,
        tool_call_id: String,
        tool_name: String,
        outcome: PermissionOutcome,
        rule_scope: Option<PermissionRuleScope>,
    },
    TaskUpdated {
        task_id: String,
        kind: TaskKind,
        status: TaskLifecycleStatus,
        title: String,
        detail: Option<String>,
    },
    CompactionStarted {
        agent_id: String,
    },
    CompactionCompleted {
        agent_id: String,
        replaced_items: usize,
        preserved_items: usize,
        resulting_transcript_len: usize,
        extracted_facts_count: usize,
        summary_preview: String,
    },
    /// How many records a memory ingest stored for an agent.
    ///
    /// Emitted when a memory ingest finishes successfully for an agent running
    /// on this session — the session's own agent after each completed turn,
    /// and any subagent it spawned. `stored_records` can be `0`: an ingest
    /// that found nothing new to store still reports that it ran. A failed
    /// ingest arrives as a [`Notice`](Self::Notice) with
    /// [`NoticeSeverity::Warning`] instead.
    ///
    /// Only this session's agents reach this channel. Memory activity from
    /// other sessions on the same runtime — or from an ingest a host drives
    /// through the runtime directly — stays off the stream, so `agent_id`
    /// distinguishes the session's own agent from its subagents, never one
    /// session from another.
    MemoryUpdated {
        agent_id: String,
        stored_records: usize,
    },
    /// Token usage report after a model response completes.
    ///
    /// `reasoning_tokens` and `thoughts_tokens` carry whichever breakdown the
    /// provider reported, with the inclusion rules described on
    /// [`AgentEvent::UsageReport`](crate::agent::AgentEvent::UsageReport).
    UsageReport {
        agent_id: String,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
        #[serde(default)]
        reasoning_tokens: u64,
        #[serde(default)]
        thoughts_tokens: u64,
    },
    Notice {
        severity: NoticeSeverity,
        message: String,
    },
    RetryAttempt {
        agent_id: String,
        error_message: String,
        attempt: u32,
        max_attempts: u32,
        next_delay_ms: u64,
    },
    Error {
        message: String,
        recoverable: bool,
    },
    /// The session returned to an earlier entry; subsequent turns continue
    /// from there along a new path.
    Branched {
        entry_id: String,
        /// How many entries left the active path. They remain in the
        /// transcript and stay reachable.
        abandoned_entries: usize,
    },
}

fn is_zero_usize(value: &usize) -> bool {
    *value == 0
}
