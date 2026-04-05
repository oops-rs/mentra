use serde::{Deserialize, Serialize};

use super::types::SessionId;

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
/// `serde_json::Value` does not implement `Eq`, so the `preview` field in
/// `PermissionRequested` is stored as a JSON `String` to preserve `Eq`
/// derivation on the entire enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    SessionStarted {
        session_id: SessionId,
    },
    UserMessage {
        text: String,
    },
    AssistantTokenDelta {
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
        /// JSON-encoded preview data. Stored as `String` because
        /// `serde_json::Value` does not implement `Eq`.
        preview: String,
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
    MemoryUpdated {
        agent_id: String,
        stored_records: usize,
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
}
