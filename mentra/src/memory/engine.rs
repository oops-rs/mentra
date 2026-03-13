use std::{
    borrow::Cow,
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    Message,
    agent::ContextCompactionTrigger,
    provider::{ContentBlock, Provider, ProviderRequestOptions, Request},
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

#[derive(Debug, Clone, PartialEq)]
pub struct MemoryHit {
    pub record_id: String,
    pub kind: MemoryRecordKind,
    pub content: String,
    pub source_revision: u64,
    pub created_at: i64,
    pub metadata_json: String,
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

#[derive(Debug, Clone)]
pub struct CompactRequest {
    pub agent_id: String,
    pub base_revision: u64,
    pub history: Vec<Message>,
    pub preserve_from: usize,
    pub trigger: ContextCompactionTrigger,
    pub transcript_dir: PathBuf,
    pub summary_max_input_chars: usize,
    pub summary_max_output_tokens: u32,
    pub model: String,
    pub provider_request_options: ProviderRequestOptions,
}

#[derive(Debug, Clone)]
pub struct CompactProposal {
    pub agent_id: String,
    pub base_revision: u64,
    pub trigger: ContextCompactionTrigger,
    pub transcript_path: PathBuf,
    pub transcript: Vec<Message>,
    pub summary: String,
    pub replaced_messages: usize,
    pub preserved_messages: usize,
}

pub trait MemoryStore: Send + Sync {
    fn upsert_records(&self, records: &[MemoryRecord]) -> Result<(), RuntimeError>;
    fn search_records(
        &self,
        agent_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, RuntimeError>;
    fn delete_records(&self, record_ids: &[String]) -> Result<(), RuntimeError>;
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

    pub async fn search(&self, request: SearchRequest) -> Result<Vec<MemoryHit>, RuntimeError> {
        let _ = self.hooks.emit(
            self.store.as_ref(),
            &RuntimeHookEvent::MemorySearchStarted {
                agent_id: request.agent_id.clone(),
                limit: request.limit,
                query_preview: preview_text(&request.query, 120),
            },
        );
        let records =
            match self
                .store
                .search_records(&request.agent_id, &request.query, request.limit)
            {
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
        let hits = records
            .into_iter()
            .map(|record| MemoryHit {
                record_id: record.record_id,
                kind: record.kind,
                content: record.content,
                source_revision: record.source_revision,
                created_at: record.created_at,
                metadata_json: record.metadata_json,
                score: record.score,
            })
            .collect::<Vec<_>>();
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

    pub async fn compact(
        &self,
        provider: Arc<dyn Provider>,
        request: CompactRequest,
    ) -> Result<Option<CompactProposal>, RuntimeError> {
        if request.history.is_empty() {
            return Ok(None);
        }

        let preserve_from = request.preserve_from.min(request.history.len());
        let summary_target = &request.history[..preserve_from];
        if summary_target.is_empty() {
            return Ok(None);
        }

        let transcript_path = persist_transcript(&request.history, &request.transcript_dir).await?;
        let summary = summarize_messages(provider, &request, summary_target).await?;
        let mut next_history = Vec::with_capacity(request.history.len() - preserve_from + 1);
        next_history.push(Message::user(ContentBlock::text(format!(
            "[Compressed context]\n\n{summary}"
        ))));
        next_history.extend_from_slice(&request.history[preserve_from..]);

        let proposal = CompactProposal {
            agent_id: request.agent_id.clone(),
            base_revision: request.base_revision,
            trigger: request.trigger,
            transcript_path: transcript_path.clone(),
            transcript: next_history,
            summary,
            replaced_messages: summary_target.len(),
            preserved_messages: request.history.len() - preserve_from,
        };

        let _ = self.hooks.emit(
            self.store.as_ref(),
            &RuntimeHookEvent::MemoryCompactionProposed {
                agent_id: request.agent_id,
                base_revision: request.base_revision,
                transcript_path,
            },
        );

        Ok(Some(proposal))
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
            score: None,
        };
        self.store.upsert_records(&[record])
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
            "[{} rev={}] {}",
            kind_label(hit.kind),
            hit.source_revision,
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

async fn persist_transcript(
    history: &[Message],
    transcript_dir: &Path,
) -> Result<PathBuf, RuntimeError> {
    tokio::fs::create_dir_all(transcript_dir)
        .await
        .map_err(RuntimeError::FailedToPersistTranscript)?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_nanos();
    let transcript_path = transcript_dir.join(format!("{timestamp}.jsonl"));
    let mut serialized = String::new();

    for message in history {
        let line =
            serde_json::to_string(message).map_err(RuntimeError::FailedToSerializeTranscript)?;
        serialized.push_str(&line);
        serialized.push('\n');
    }

    tokio::fs::write(&transcript_path, serialized)
        .await
        .map_err(RuntimeError::FailedToPersistTranscript)?;

    Ok(transcript_path)
}

async fn summarize_messages(
    provider: Arc<dyn Provider>,
    request: &CompactRequest,
    messages: &[Message],
) -> Result<String, RuntimeError> {
    let serialized =
        serde_json::to_string(messages).map_err(RuntimeError::FailedToSerializeTranscript)?;
    let transcript = truncate_to_char_boundary(&serialized, request.summary_max_input_chars);
    let system = "You compress agent conversations for continuity. Preserve the user goal, key decisions, relevant code paths, important tool outputs, open questions, and remaining work. Keep it concise and factual.";
    let prompt = format!(
        "Summarize this conversation for continuity. The summary should help a future model continue the work without the full transcript.\n\nTranscript JSON:\n{transcript}"
    );

    let response = provider
        .send(Request {
            model: Cow::Borrowed(request.model.as_str()),
            system: Some(Cow::Borrowed(system)),
            messages: Cow::Owned(vec![Message::user(ContentBlock::text(prompt))]),
            tools: Cow::Owned(Vec::new()),
            tool_choice: None,
            temperature: None,
            max_output_tokens: Some(request.summary_max_output_tokens),
            metadata: Cow::Owned(Default::default()),
            provider_request_options: request.provider_request_options.clone(),
        })
        .await
        .map_err(RuntimeError::FailedToCompactHistory)?;

    let summary = response
        .content
        .into_iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    if summary.is_empty() {
        Ok("No additional summary was produced.".to_string())
    } else {
        Ok(summary)
    }
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
            ContentBlock::Image { .. } => None,
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
