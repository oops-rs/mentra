use std::{
    collections::HashSet,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    Message,
    provider::ContentBlock,
    runtime::{RuntimeError, RuntimeHookEvent, RuntimeHooks, RuntimeStore, TaskItem},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryRecordKind {
    Episode,
    Summary,
    Fact,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub record_id: String,
    pub agent_id: String,
    pub kind: MemoryRecordKind,
    pub content: String,
    pub source_revision: u64,
    pub created_at: i64,
    pub metadata_json: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryCursor {
    pub last_ingested_revision: u64,
}

#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub agent_id: String,
    pub query: String,
    pub limit: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MemorySearchMode {
    #[default]
    Automatic,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySearchRequest {
    pub agent_id: String,
    pub query: String,
    pub limit: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub char_budget: Option<usize>,
    #[serde(default)]
    pub mode: MemorySearchMode,
}

impl From<SearchRequest> for MemorySearchRequest {
    fn from(value: SearchRequest) -> Self {
        Self {
            agent_id: value.agent_id,
            query: value.query,
            limit: value.limit,
            char_budget: None,
            mode: MemorySearchMode::Automatic,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MemoryHit {
    pub record_id: String,
    pub kind: MemoryRecordKind,
    pub content: String,
    pub source_revision: u64,
    pub created_at: i64,
    pub metadata_json: String,
    pub source: Option<String>,
    pub why_retrieved: Option<String>,
    pub score: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct IngestRequest {
    pub agent_id: String,
    pub source_revision: u64,
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IngestOutcome {
    pub stored_records: usize,
    pub skipped: bool,
}

pub trait MemoryStore: Send + Sync {
    fn upsert_records(&self, records: &[MemoryRecord]) -> Result<(), RuntimeError>;
    fn search_records_with_options(
        &self,
        request: &MemorySearchRequest,
    ) -> Result<Vec<MemoryRecord>, RuntimeError>;
    fn search_records(
        &self,
        agent_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, RuntimeError> {
        self.search_records_with_options(&MemorySearchRequest {
            agent_id: agent_id.to_string(),
            query: query.to_string(),
            limit,
            char_budget: None,
            mode: MemorySearchMode::Automatic,
        })
    }
    fn delete_records(&self, record_ids: &[String]) -> Result<(), RuntimeError>;
    fn tombstone_records(
        &self,
        agent_id: &str,
        record_ids: &[String],
    ) -> Result<usize, RuntimeError>;
    fn load_agent_memory_cursor(
        &self,
        agent_id: &str,
    ) -> Result<Option<MemoryCursor>, RuntimeError>;
    fn save_agent_memory_cursor(
        &self,
        agent_id: &str,
        cursor: &MemoryCursor,
    ) -> Result<(), RuntimeError>;
}

#[derive(Clone)]
pub struct MemoryEngine {
    store: Arc<dyn RuntimeStore>,
    hooks: RuntimeHooks,
}

impl MemoryEngine {
    pub fn new(store: Arc<dyn RuntimeStore>, hooks: RuntimeHooks) -> Self {
        Self { store, hooks }
    }

    pub async fn search(
        &self,
        request: impl Into<MemorySearchRequest>,
    ) -> Result<Vec<MemoryHit>, RuntimeError> {
        let request = request.into();
        let _ = self.hooks.emit(
            self.store.as_ref(),
            &RuntimeHookEvent::MemorySearchStarted {
                agent_id: request.agent_id.clone(),
                limit: request.limit,
                query_preview: preview_text(&request.query, 120),
            },
        );
        let records = match self.store.search_records_with_options(&request) {
            Ok(records) => records,
            Err(error) => {
                let _ = self.hooks.emit(
                    self.store.as_ref(),
                    &RuntimeHookEvent::MemorySearchFinished {
                        agent_id: request.agent_id,
                        success: false,
                        result_count: 0,
                        error: Some(error.to_string()),
                    },
                );
                return Err(error);
            }
        };
        let mut hits = records
            .into_iter()
            .map(|record| {
                let why_retrieved = build_why_retrieved(&request.query, &record);
                MemoryHit {
                    record_id: record.record_id,
                    kind: record.kind,
                    content: record.content,
                    source_revision: record.source_revision,
                    created_at: record.created_at,
                    metadata_json: record.metadata_json,
                    source: record.source,
                    why_retrieved,
                    score: record.score,
                }
            })
            .collect::<Vec<_>>();
        if let Some(char_budget) = request.char_budget {
            trim_hits_to_char_budget(&mut hits, char_budget);
        }
        let _ = self.hooks.emit(
            self.store.as_ref(),
            &RuntimeHookEvent::MemorySearchFinished {
                agent_id: request.agent_id,
                success: true,
                result_count: hits.len(),
                error: None,
            },
        );
        Ok(hits)
    }

    pub fn schedule_ingest(&self, request: IngestRequest) {
        let engine = self.clone();
        tokio::spawn(async move {
            let _ = engine.ingest(request).await;
        });
    }

    pub async fn ingest(&self, request: IngestRequest) -> Result<IngestOutcome, RuntimeError> {
        let _ = self.hooks.emit(
            self.store.as_ref(),
            &RuntimeHookEvent::MemoryIngestStarted {
                agent_id: request.agent_id.clone(),
                source_revision: request.source_revision,
            },
        );

        let cursor = match self.store.load_agent_memory_cursor(&request.agent_id) {
            Ok(cursor) => cursor.unwrap_or_default(),
            Err(error) => {
                let _ = self.hooks.emit(
                    self.store.as_ref(),
                    &RuntimeHookEvent::MemoryIngestFinished {
                        agent_id: request.agent_id,
                        source_revision: request.source_revision,
                        success: false,
                        stored_records: 0,
                        error: Some(error.to_string()),
                    },
                );
                return Err(error);
            }
        };
        if cursor.last_ingested_revision >= request.source_revision {
            let _ = self.hooks.emit(
                self.store.as_ref(),
                &RuntimeHookEvent::MemoryIngestFinished {
                    agent_id: request.agent_id,
                    source_revision: request.source_revision,
                    success: true,
                    stored_records: 0,
                    error: None,
                },
            );
            return Ok(IngestOutcome {
                stored_records: 0,
                skipped: true,
            });
        }

        let episode = summarize_episode(&request.messages);
        if episode.is_empty() {
            if let Err(error) = self.store.save_agent_memory_cursor(
                &request.agent_id,
                &MemoryCursor {
                    last_ingested_revision: request.source_revision,
                },
            ) {
                let _ = self.hooks.emit(
                    self.store.as_ref(),
                    &RuntimeHookEvent::MemoryIngestFinished {
                        agent_id: request.agent_id,
                        source_revision: request.source_revision,
                        success: false,
                        stored_records: 0,
                        error: Some(error.to_string()),
                    },
                );
                return Err(error);
            }
            let _ = self.hooks.emit(
                self.store.as_ref(),
                &RuntimeHookEvent::MemoryIngestFinished {
                    agent_id: request.agent_id,
                    source_revision: request.source_revision,
                    success: true,
                    stored_records: 0,
                    error: None,
                },
            );
            return Ok(IngestOutcome {
                stored_records: 0,
                skipped: false,
            });
        }

        let record = MemoryRecord {
            record_id: format!("episode:{}:{}", request.agent_id, request.source_revision),
            agent_id: request.agent_id.clone(),
            kind: MemoryRecordKind::Episode,
            content: episode,
            source_revision: request.source_revision,
            created_at: now_secs(),
            metadata_json: "{}".to_string(),
            source: Some("auto_ingest".to_string()),
            pinned: false,
            score: None,
        };
        if let Err(error) = self.store.upsert_records(&[record]) {
            let _ = self.hooks.emit(
                self.store.as_ref(),
                &RuntimeHookEvent::MemoryIngestFinished {
                    agent_id: request.agent_id,
                    source_revision: request.source_revision,
                    success: false,
                    stored_records: 0,
                    error: Some(error.to_string()),
                },
            );
            return Err(error);
        }
        if let Err(error) = self.store.save_agent_memory_cursor(
            &request.agent_id,
            &MemoryCursor {
                last_ingested_revision: request.source_revision,
            },
        ) {
            let _ = self.hooks.emit(
                self.store.as_ref(),
                &RuntimeHookEvent::MemoryIngestFinished {
                    agent_id: request.agent_id,
                    source_revision: request.source_revision,
                    success: false,
                    stored_records: 0,
                    error: Some(error.to_string()),
                },
            );
            return Err(error);
        }
        let _ = self.hooks.emit(
            self.store.as_ref(),
            &RuntimeHookEvent::MemoryIngestFinished {
                agent_id: request.agent_id,
                source_revision: request.source_revision,
                success: true,
                stored_records: 1,
                error: None,
            },
        );
        Ok(IngestOutcome {
            stored_records: 1,
            skipped: false,
        })
    }

    pub fn store_compaction_summary(
        &self,
        agent_id: &str,
        source_revision: u64,
        summary: &str,
    ) -> Result<(), RuntimeError> {
        let record = MemoryRecord {
            record_id: format!("summary:{agent_id}:{source_revision}"),
            agent_id: agent_id.to_string(),
            kind: MemoryRecordKind::Summary,
            content: summary.to_string(),
            source_revision,
            created_at: now_secs(),
            metadata_json: "{}".to_string(),
            source: Some("auto_compaction".to_string()),
            pinned: false,
            score: None,
        };
        self.store.upsert_records(&[record])
    }

    pub fn pin(
        &self,
        agent_id: &str,
        source_revision: u64,
        content: &str,
    ) -> Result<MemoryRecord, RuntimeError> {
        let record = MemoryRecord {
            record_id: format!("fact:{agent_id}:manual:{}", now_nanos()),
            agent_id: agent_id.to_string(),
            kind: MemoryRecordKind::Fact,
            content: content.trim().to_string(),
            source_revision,
            created_at: now_secs(),
            metadata_json: r#"{"origin":"manual_pin"}"#.to_string(),
            source: Some("manual_pin".to_string()),
            pinned: true,
            score: None,
        };
        self.store.upsert_records(std::slice::from_ref(&record))?;
        Ok(record)
    }

    pub fn forget(&self, agent_id: &str, record_id: &str) -> Result<bool, RuntimeError> {
        self.store
            .tombstone_records(agent_id, &[record_id.to_string()])
            .map(|count| count > 0)
    }
}

pub(crate) fn build_search_query(history: &[Message], tasks: &[TaskItem]) -> String {
    let mut parts = history.iter().rev().take(6).collect::<Vec<_>>();
    parts.reverse();

    let mut query = parts
        .into_iter()
        .flat_map(message_to_lines)
        .collect::<Vec<_>>()
        .join("\n");

    let unfinished = tasks
        .iter()
        .filter(|task| !matches!(task.status, crate::runtime::TaskStatus::Completed))
        .map(|task| {
            let description = task.description.trim();
            if description.is_empty() {
                task.subject.clone()
            } else {
                format!("{}: {}", task.subject, description)
            }
        })
        .collect::<Vec<_>>();
    if !unfinished.is_empty() {
        if !query.is_empty() {
            query.push('\n');
        }
        query.push_str("Tasks:\n");
        query.push_str(&unfinished.join("\n"));
    }
    query
}

pub(crate) fn recalled_memory_message(hits: &[MemoryHit], char_limit: usize) -> Option<Message> {
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    let mut used = 0usize;

    for hit in hits {
        if !seen.insert(hit.record_id.clone()) {
            continue;
        }
        let line = format!(
            "[{} rev={}{}{}] {}",
            kind_label(hit.kind),
            hit.source_revision,
            hit.source
                .as_deref()
                .map(|source| format!(" source={source}"))
                .unwrap_or_default(),
            hit.why_retrieved
                .as_deref()
                .map(|why| format!(" why={why}"))
                .unwrap_or_default(),
            hit.content.trim()
        );
        if line.trim().is_empty() {
            continue;
        }
        if !entries.is_empty() && used + line.len() + 1 > char_limit {
            break;
        }
        used += if entries.is_empty() {
            line.len()
        } else {
            line.len() + 1
        };
        entries.push(line);
    }

    if entries.is_empty() {
        return None;
    }

    Some(Message::user(ContentBlock::text(format!(
        "<recalled-memory>\n{}\n</recalled-memory>",
        entries.join("\n")
    ))))
}

fn summarize_episode(messages: &[Message]) -> String {
    let mut lines = Vec::new();
    for message in messages {
        let label = match message.role {
            crate::Role::User => "user",
            crate::Role::Assistant => "assistant",
            crate::Role::Unknown(_) => "unknown",
        };
        for line in message_to_lines(message) {
            lines.push(format!("{label}: {line}"));
        }
    }
    lines.join("\n")
}

fn message_to_lines(message: &Message) -> Vec<String> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.trim().to_string()),
            ContentBlock::ToolUse { name, input, .. } => Some(format!("tool use {name} {input}")),
            ContentBlock::ToolResult { content, .. } => Some(format!("tool result {content}")),
            ContentBlock::Image { .. }
            | ContentBlock::HostedToolSearch { .. }
            | ContentBlock::HostedWebSearch { .. }
            | ContentBlock::ImageGeneration { .. } => None,
        })
        .filter(|text| !text.is_empty())
        .collect()
}

fn kind_label(kind: MemoryRecordKind) -> &'static str {
    match kind {
        MemoryRecordKind::Episode => "episode",
        MemoryRecordKind::Summary => "summary",
        MemoryRecordKind::Fact => "fact",
    }
}

fn preview_text(text: &str, limit: usize) -> String {
    truncate_to_char_boundary(text.trim(), limit).to_string()
}

fn truncate_to_char_boundary(input: &str, max_chars: usize) -> &str {
    if input.chars().count() <= max_chars {
        return input;
    }

    let mut end = input.len();
    for (index, _) in input.char_indices().take(max_chars + 1) {
        end = index;
    }
    &input[..end]
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn trim_hits_to_char_budget(hits: &mut Vec<MemoryHit>, char_budget: usize) {
    if char_budget == 0 {
        hits.clear();
        return;
    }

    let mut kept = Vec::with_capacity(hits.len());
    let mut used = 0usize;
    for hit in hits.drain(..) {
        let line_len = hit.content.len()
            + hit.source.as_deref().map_or(0, str::len)
            + hit.why_retrieved.as_deref().map_or(0, str::len);
        if !kept.is_empty() && used + line_len > char_budget {
            break;
        }
        used += line_len;
        kept.push(hit);
    }
    *hits = kept;
}

fn build_why_retrieved(query: &str, record: &MemoryRecord) -> Option<String> {
    let mut reasons = Vec::new();
    let matched = query
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .filter(|token| {
            record
                .content
                .to_lowercase()
                .contains(&token.to_lowercase())
        })
        .take(2)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if !matched.is_empty() {
        reasons.push(format!("matched {}", matched.join(",")));
    }
    match record.kind {
        MemoryRecordKind::Fact => reasons.push("fact".to_string()),
        MemoryRecordKind::Summary => reasons.push("summary".to_string()),
        MemoryRecordKind::Episode => {}
    }
    if record.pinned || record.source.as_deref() == Some("manual_pin") {
        reasons.push("manual".to_string());
    }
    if reasons.is_empty() {
        None
    } else {
        Some(reasons.join("; "))
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}
