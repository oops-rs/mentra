#[cfg(test)]
mod tests;

use std::{
    borrow::Cow,
    collections::HashSet,
    path::Path,
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use regex::Regex;

use crate::{
    ContentBlock, Message,
    error::RuntimeError,
    provider::{
        CompactionInputItem, CompactionRequest as ProviderCompactionRequest,
        CompactionResponse as ProviderCompactionResponse, Provider, ProviderError,
        ProviderRequestOptions, Request,
    },
    transcript::{AgentTranscript, CompactionSummary, TranscriptItem, TranscriptKind},
};

/// Context mechanically extracted from transcript items before summarization.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtractedContext {
    pub files_touched: Vec<String>,
    pub verification_outcomes: Vec<String>,
    pub permission_decisions: Vec<String>,
}

/// Scan transcript items to extract file paths, verification outcomes, and permission decisions.
pub fn extract_context(items: &[TranscriptItem]) -> ExtractedContext {
    use std::sync::LazyLock;

    static FILE_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?:^|[\s"'`(,])([a-zA-Z0-9_.][a-zA-Z0-9_./\-]*\.[a-zA-Z]{1,10})"#)
            .expect("valid regex literal")
    });
    static VERIFICATION_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)(cargo\s+test|pytest|npm\s+test|jest|mocha|go\s+test|make\s+test|rspec|yarn\s+test).*?(pass|fail|error|ok|success|FAILED|PASSED)",
        )
        .expect("valid regex literal")
    });
    static PERMISSION_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(permission|allowed|denied|approved|rejected|authorized)")
            .expect("valid regex literal")
    });
    let file_re = &*FILE_RE;
    let verification_re = &*VERIFICATION_RE;
    let permission_re = &*PERMISSION_RE;

    let mut files_seen = HashSet::new();
    let mut files = Vec::new();
    let mut verifications = Vec::new();
    let mut permissions = Vec::new();

    for item in items {
        let text = item.text();
        let is_tool_exchange = matches!(item.kind, TranscriptKind::ToolExchange { .. });

        if is_tool_exchange {
            for cap in file_re.captures_iter(&text) {
                if let Some(m) = cap.get(1) {
                    let path = m.as_str().to_string();
                    if files_seen.insert(path.clone()) {
                        files.push(path);
                    }
                }
            }
        }

        for line in text.lines() {
            if verification_re.is_match(line) {
                let trimmed = line.trim().to_string();
                if !trimmed.is_empty() {
                    verifications.push(trimmed);
                }
            }
            if permission_re.is_match(line) {
                let trimmed = line.trim().to_string();
                if !trimmed.is_empty() {
                    permissions.push(trimmed);
                }
            }
        }
    }

    ExtractedContext {
        files_touched: files,
        verification_outcomes: verifications,
        permission_decisions: permissions,
    }
}

/// Format extracted context as a text preamble for the compaction prompt.
pub fn format_extracted_context(ctx: &ExtractedContext) -> String {
    let mut sections = Vec::new();

    if !ctx.files_touched.is_empty() {
        let mut section = String::from("FILES TOUCHED (must preserve):\n");
        for f in &ctx.files_touched {
            section.push_str("- ");
            section.push_str(f);
            section.push('\n');
        }
        sections.push(section);
    }

    if !ctx.verification_outcomes.is_empty() {
        let mut section = String::from("VERIFICATION OUTCOMES (must preserve):\n");
        for v in &ctx.verification_outcomes {
            section.push_str("- ");
            section.push_str(v);
            section.push('\n');
        }
        sections.push(section);
    }

    if !ctx.permission_decisions.is_empty() {
        let mut section = String::from("PERMISSION DECISIONS (must preserve):\n");
        for p in &ctx.permission_decisions {
            section.push_str("- ");
            section.push_str(p);
            section.push('\n');
        }
        sections.push(section);
    }

    sections.join("\n")
}

/// Diagnostics captured during a compaction operation.
#[derive(Debug, Clone)]
pub struct CompactionDiagnostics {
    pub items_before: usize,
    pub items_after: usize,
    pub approx_tokens_before: usize,
    pub approx_tokens_after: usize,
    pub preserved_user_turns: usize,
    pub preserved_delegation_results: usize,
    pub extracted_facts_count: usize,
    pub summary_preview: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CompactionMode {
    #[default]
    LocalOnly,
    PreferRemote,
    RemoteOnly,
}

#[derive(Debug, Clone)]
pub struct CompactionRequest {
    pub model: String,
    pub transcript: AgentTranscript,
    pub transcript_dir: PathBuf,
    pub summary_max_input_chars: usize,
    pub summary_max_output_tokens: u32,
    pub preserve_recent_user_tokens: usize,
    pub preserve_recent_delegation_results: usize,
    pub provider_request_options: ProviderRequestOptions,
    pub mode: CompactionMode,
    pub max_persisted_transcripts: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionExecutionMode {
    Local,
    Remote,
}

/// The result of one compaction: a replacement [`AgentTranscript`] plus
/// counts describing how it was built from the original.
///
/// **Metadata-preservation guarantee (mentra ADR-0001 §6).** Every item that
/// survives into `transcript` — the untouched continuation tail, salvaged
/// recent user turns, and salvaged recent delegation results — is copied
/// **verbatim** from the original transcript, so any opaque
/// [`TranscriptItem::details`] a host attached survives bit-for-bit. This
/// holds by construction: mentra never rebuilds a preserved item from its
/// projected [`crate::Message`] (which would drop `details`, a field that
/// exists only on `TranscriptItem`); it clones the original item.
///
/// This guarantee is scoped to *preserved* items only. An item inside the
/// summarized prefix that is **not** salvaged is replaced by the
/// [`CompactionSummary`] along with the rest of its content — its `details`
/// go with it. That is honest, documented behavior, not a violation: the
/// contract never promises to resurrect a discarded item's metadata, only to
/// never silently drop it from one that was kept. The full pre-compaction
/// transcript — discarded items included — is written to `transcript_path`
/// before summarization runs, so a host that needs a discarded item's
/// `details` after the fact can still recover them from that snapshot.
#[derive(Debug, Clone)]
pub struct CompactionOutcome {
    pub mode: CompactionExecutionMode,
    /// Path to the `.jsonl` snapshot of the **entire pre-compaction**
    /// transcript, one [`TranscriptItem`] per line, written before
    /// summarization runs. Every item's `details` round-trips through this
    /// file, including items the compacted `transcript` goes on to discard —
    /// this is the recovery artifact for the summarized prefix.
    pub transcript_path: PathBuf,
    pub transcript: AgentTranscript,
    pub summary: CompactionSummary,
    /// Count of original items in the summarized prefix
    /// (`required_tail_start_for_continuation`'s `preserve_from` split).
    /// This counts every item in that prefix, **including** ones also
    /// salvaged into `transcript` by `preserved_user_turns` /
    /// `preserved_delegation_results` — from this count's point of view they
    /// were replaced by the summary, even though their content (and
    /// `details`) survives verbatim elsewhere in the replacement transcript.
    /// It does not mean "gone".
    pub replaced_items: usize,
    /// Count of items kept strictly because they are the untouched
    /// continuation tail (outside the summarized prefix), independent of
    /// `preserved_user_turns` / `preserved_delegation_results` below, which
    /// count salvaged items pulled *out of* the summarized prefix instead.
    /// The three counts are disjoint by construction.
    pub preserved_items: usize,
    /// Count of recent user turns salvaged out of the summarized prefix and
    /// copied verbatim (details included) into the replacement transcript.
    pub preserved_user_turns: usize,
    /// Count of recent delegation results salvaged out of the summarized
    /// prefix and copied verbatim (details included) into the replacement
    /// transcript.
    pub preserved_delegation_results: usize,
    pub diagnostics: CompactionDiagnostics,
}

/// Compacts an agent transcript into a shorter one carrying a summary of the
/// discarded portion. See [`CompactionOutcome`] for the metadata-preservation
/// contract every implementation must uphold: `details` on any item that
/// survives compaction (tail, salvaged user turns, salvaged delegation
/// results) is preserved bit-for-bit; `details` on a discarded, unsalvaged
/// item is honestly gone with the rest of that item's content, recoverable
/// only from the pre-compaction snapshot at [`CompactionOutcome::transcript_path`].
#[async_trait]
pub trait CompactionEngine: Send + Sync {
    async fn compact(
        &self,
        provider: Arc<dyn Provider>,
        request: CompactionRequest,
    ) -> Result<Option<CompactionOutcome>, RuntimeError>;
}

/// The default [`CompactionEngine`]: summarizes the compactable prefix of a
/// transcript (locally via the provider's chat completion, or remotely via
/// [`Provider::compact`] when supported), while keeping the continuation
/// tail and a bounded number of recent user turns and delegation results
/// verbatim — including their opaque `details` — per the
/// [`CompactionOutcome`] contract.
#[derive(Debug, Default)]
pub struct StandardCompactionEngine;

#[async_trait]
impl CompactionEngine for StandardCompactionEngine {
    async fn compact(
        &self,
        provider: Arc<dyn Provider>,
        request: CompactionRequest,
    ) -> Result<Option<CompactionOutcome>, RuntimeError> {
        if request.transcript.is_empty() {
            return Ok(None);
        }

        let preserve_from = required_tail_start_for_continuation(request.transcript.items());
        if preserve_from == 0 {
            return Ok(None);
        }
        let compacted_prefix = &request.transcript.items()[..preserve_from];
        if compacted_prefix.is_empty() {
            return Ok(None);
        }

        let transcript_path =
            persist_transcript(request.transcript.items(), &request.transcript_dir).await?;
        if let Some(max) = request.max_persisted_transcripts {
            let _ = cleanup_old_transcripts(&request.transcript_dir, max).await;
        }
        let supports_remote = provider.capabilities().supports_history_compaction;
        let (mode, summary) = match request.mode {
            CompactionMode::LocalOnly => (
                CompactionExecutionMode::Local,
                summarize_locally(provider, &request, compacted_prefix).await?,
            ),
            CompactionMode::PreferRemote => {
                if supports_remote {
                    match compact_remotely(provider.clone(), &request, compacted_prefix).await {
                        Ok(Some(summary)) => (CompactionExecutionMode::Remote, summary),
                        Ok(None)
                        | Err(RuntimeError::FailedToCompactHistory(
                            ProviderError::UnsupportedCapability(_),
                        )) => (
                            CompactionExecutionMode::Local,
                            summarize_locally(provider, &request, compacted_prefix).await?,
                        ),
                        Err(error) => return Err(error),
                    }
                } else {
                    (
                        CompactionExecutionMode::Local,
                        summarize_locally(provider, &request, compacted_prefix).await?,
                    )
                }
            }
            CompactionMode::RemoteOnly => {
                if !supports_remote {
                    return Err(RuntimeError::FailedToCompactHistory(
                        ProviderError::UnsupportedCapability("history_compaction".to_string()),
                    ));
                }
                (
                    CompactionExecutionMode::Remote,
                    compact_remotely(provider, &request, compacted_prefix)
                        .await?
                        .ok_or_else(|| {
                            RuntimeError::FailedToCompactHistory(
                                ProviderError::UnsupportedCapability(
                                    "history_compaction".to_string(),
                                ),
                            )
                        })?,
                )
            }
        };

        let items_before = request.transcript.len();
        let tokens_before = approx_token_count_items(request.transcript.items());

        let preserved_user_turns =
            select_recent_user_turns(compacted_prefix, request.preserve_recent_user_tokens);
        let preserved_delegation_results = select_recent_delegation_results(
            compacted_prefix,
            request.preserve_recent_delegation_results,
        );

        let extracted = extract_context(compacted_prefix);
        let extracted_facts_count = extracted.files_touched.len()
            + extracted.verification_outcomes.len()
            + extracted.permission_decisions.len();

        let mut replacement = Vec::new();
        replacement.extend(preserved_user_turns.iter().cloned());
        for item in &preserved_delegation_results {
            if !replacement.contains(item) {
                replacement.push(item.clone());
            }
        }
        replacement.push(TranscriptItem::compaction_summary(summary.clone()));
        replacement.extend_from_slice(&request.transcript.items()[preserve_from..]);

        let items_after = replacement.len();
        let tokens_after = approx_token_count_items(&replacement);

        let summary_preview = summary
            .render_for_handoff()
            .chars()
            .take(200)
            .collect::<String>();

        let diagnostics = CompactionDiagnostics {
            items_before,
            items_after,
            approx_tokens_before: tokens_before,
            approx_tokens_after: tokens_after,
            preserved_user_turns: preserved_user_turns.len(),
            preserved_delegation_results: preserved_delegation_results.len(),
            extracted_facts_count,
            summary_preview,
        };

        Ok(Some(CompactionOutcome {
            mode,
            transcript_path,
            transcript: AgentTranscript::new(replacement),
            summary,
            replaced_items: compacted_prefix.len(),
            preserved_items: request.transcript.len().saturating_sub(preserve_from),
            preserved_user_turns: preserved_user_turns.len(),
            preserved_delegation_results: preserved_delegation_results.len(),
            diagnostics,
        }))
    }
}

pub(crate) fn compaction_request_from_agent(
    model: &str,
    transcript: AgentTranscript,
    config: &crate::agent::CompactionConfig,
    provider_request_options: ProviderRequestOptions,
) -> CompactionRequest {
    CompactionRequest {
        model: model.to_string(),
        transcript,
        transcript_dir: config.transcript_dir.clone(),
        summary_max_input_chars: config.summary_max_input_chars,
        summary_max_output_tokens: config.summary_max_output_tokens,
        preserve_recent_user_tokens: config.preserve_recent_user_tokens,
        preserve_recent_delegation_results: config.preserve_recent_delegation_results,
        provider_request_options,
        mode: config.mode,
        max_persisted_transcripts: config.max_persisted_transcripts,
    }
}

async fn summarize_locally(
    provider: Arc<dyn Provider>,
    request: &CompactionRequest,
    items: &[TranscriptItem],
) -> Result<CompactionSummary, RuntimeError> {
    let summary_items = items_without_thinking(items);
    let serialized =
        serde_json::to_string(&summary_items).map_err(RuntimeError::FailedToSerializeTranscript)?;
    let transcript = truncate_to_char_boundary(&serialized, request.summary_max_input_chars);

    let extracted = extract_context(items);
    let context_preamble = format_extracted_context(&extracted);

    let system = "\
You are a coding-session compaction engine. Your job is to compress an agent transcript \
into a structured JSON summary that preserves all operationally critical context for \
session continuity.\n\n\
You MUST preserve:\n\
- All file paths that were read, written, or modified\n\
- Shell command outcomes (build results, test pass/fail, lint output)\n\
- Permission decisions (what was allowed, denied, or deferred)\n\
- Architectural decisions and their rationale\n\
- Constraints and invariants discovered during the session\n\
- Current working state (what is done, what is in progress, what remains)\n\
- Error states and how they were resolved\n\
- Delegated work outcomes and pending delegations\n\n\
Return strict JSON with keys: goal, progress, decisions, constraints, \
delegated_work, artifacts, open_questions, next_steps.\n\
Each key should contain concrete, specific information -- not vague summaries.\n\
File paths, command outputs, and error messages should be quoted verbatim.";

    let mut prompt = String::new();
    if !context_preamble.is_empty() {
        prompt.push_str("=== EXTRACTED FACTS (must preserve verbatim) ===\n");
        prompt.push_str(&context_preamble);
        prompt.push_str("\n=== END EXTRACTED FACTS ===\n\n");
    }
    prompt.push_str("Summarize this agent transcript for continuity and multi-agent handoff. Preserve goal, progress, concrete decisions, constraints, delegated work outcomes, artifacts, open questions, and next steps.\n\nTranscript JSON:\n");
    prompt.push_str(transcript);
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
    let text = response
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
    if text.is_empty() {
        return Ok(CompactionSummary::default());
    }

    serde_json::from_str(&text)
        .unwrap_or_else(|_| CompactionSummary::from_fallback_text(text))
        .pipe(Ok)
}

fn items_without_thinking(items: &[TranscriptItem]) -> Vec<TranscriptItem> {
    items
        .iter()
        .cloned()
        .map(|mut item| {
            if let Some(message) = item.message.as_mut() {
                message
                    .content
                    .retain(|block| !matches!(block, ContentBlock::Thinking { .. }));
            }
            item
        })
        .collect()
}

async fn compact_remotely(
    provider: Arc<dyn Provider>,
    request: &CompactionRequest,
    items: &[TranscriptItem],
) -> Result<Option<CompactionSummary>, RuntimeError> {
    let input = items
        .iter()
        .map(project_compaction_item)
        .collect::<Vec<_>>();
    let response = provider
        .compact(ProviderCompactionRequest {
            model: Cow::Borrowed(request.model.as_str()),
            instructions: Cow::Borrowed(
                "Compact this transcript into a continuity handoff that preserves delegated work.",
            ),
            input: Cow::Owned(input),
            metadata: Cow::Owned(Default::default()),
            provider_request_options: request.provider_request_options.clone(),
        })
        .await
        .map_err(RuntimeError::FailedToCompactHistory)?;
    Ok(parse_remote_summary(response))
}

fn parse_remote_summary(response: ProviderCompactionResponse) -> Option<CompactionSummary> {
    response
        .output
        .into_iter()
        .rev()
        .find_map(|item| match item {
            CompactionInputItem::CompactionSummary { content } => serde_json::from_str(&content)
                .ok()
                .or_else(|| Some(CompactionSummary::from_fallback_text(content))),
            _ => None,
        })
}

fn project_compaction_item(item: &TranscriptItem) -> CompactionInputItem {
    match &item.kind {
        TranscriptKind::UserTurn => CompactionInputItem::UserTurn {
            content: item.text(),
        },
        TranscriptKind::AssistantTurn => CompactionInputItem::AssistantTurn {
            content: item.text(),
        },
        TranscriptKind::ToolExchange { is_error, .. } => CompactionInputItem::ToolExchange {
            request: None,
            result: item.text(),
            is_error: *is_error,
        },
        TranscriptKind::CanonicalContext => CompactionInputItem::CanonicalContext {
            content: item.text(),
        },
        TranscriptKind::MemoryRecall => CompactionInputItem::MemoryRecall {
            content: item.text(),
        },
        TranscriptKind::DelegationRequest { delegation, .. }
        | TranscriptKind::DelegationResult { delegation, .. } => {
            CompactionInputItem::DelegationResult {
                agent_id: delegation.agent_id.clone(),
                agent_name: delegation.agent_name.clone(),
                role: delegation.role.clone(),
                status: format!("{:?}", delegation.status).to_lowercase(),
                content: item.text(),
            }
        }
        TranscriptKind::CompactionSummary { summary } => CompactionInputItem::CompactionSummary {
            content: summary.render_for_handoff(),
        },
    }
}

fn select_recent_user_turns(items: &[TranscriptItem], token_budget: usize) -> Vec<TranscriptItem> {
    let mut selected = Vec::new();
    let mut remaining = token_budget;
    for item in items.iter().rev() {
        if !item.is_real_user_turn() {
            continue;
        }
        let tokens = approx_token_count(&item.text());
        if tokens > remaining && !selected.is_empty() {
            break;
        }
        remaining = remaining.saturating_sub(tokens);
        selected.push(item.clone());
        if remaining == 0 {
            break;
        }
    }
    selected.reverse();
    selected
}

fn select_recent_delegation_results(
    items: &[TranscriptItem],
    max_items: usize,
) -> Vec<TranscriptItem> {
    let mut selected = items
        .iter()
        .filter(|item| item.is_delegation_result())
        .rev()
        .take(max_items)
        .cloned()
        .collect::<Vec<_>>();
    selected.reverse();
    selected
}

fn required_tail_start_for_continuation(items: &[TranscriptItem]) -> usize {
    let Some(last_index) = items.len().checked_sub(1) else {
        return 0;
    };
    let last = &items[last_index];
    if matches!(last.kind, TranscriptKind::ToolExchange { .. })
        && last_index > 0
        && matches!(items[last_index - 1].kind, TranscriptKind::AssistantTurn)
    {
        last_index - 1
    } else {
        last_index
    }
}

fn approx_token_count(text: &str) -> usize {
    let char_estimate = text.chars().count().div_ceil(4);
    let word_count = text.split_whitespace().count();
    let word_estimate = ((word_count as f64) * 1.3).ceil() as usize;
    char_estimate.max(word_estimate)
}

fn approx_token_count_items(items: &[TranscriptItem]) -> usize {
    items
        .iter()
        .map(|item| approx_token_count(&item.text()))
        .sum()
}

async fn persist_transcript(
    transcript: &[TranscriptItem],
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
    for item in transcript {
        let line =
            serde_json::to_string(item).map_err(RuntimeError::FailedToSerializeTranscript)?;
        serialized.push_str(&line);
        serialized.push('\n');
    }
    tokio::fs::write(&transcript_path, serialized)
        .await
        .map_err(RuntimeError::FailedToPersistTranscript)?;
    Ok(transcript_path)
}

/// Removes the oldest transcript files in `dir` when count exceeds `keep`.
/// Files are sorted by filename (nanosecond timestamps → oldest first).
/// Delete errors are ignored — this is best-effort cleanup.
pub(crate) async fn cleanup_old_transcripts(dir: &Path, keep: usize) -> Result<(), RuntimeError> {
    let mut read_dir = tokio::fs::read_dir(dir)
        .await
        .map_err(RuntimeError::FailedToPersistTranscript)?;

    let mut files: Vec<std::path::PathBuf> = Vec::new();
    while let Some(entry) = read_dir
        .next_entry()
        .await
        .map_err(RuntimeError::FailedToPersistTranscript)?
    {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            files.push(path);
        }
    }

    if files.len() <= keep {
        return Ok(());
    }

    // Sort ascending by filename — nanosecond timestamps put oldest first.
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

    let to_delete = files.len() - keep;
    for path in files.iter().take(to_delete) {
        let _ = tokio::fs::remove_file(path).await;
    }

    Ok(())
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

trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}

impl<T> Pipe for T {}
