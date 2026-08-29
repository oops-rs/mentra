use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
};

use serde_json::{Value, json};

use super::*;
use crate::{
    ContentBlock, DelegationArtifact, DelegationKind, DelegationStatus, Message, ModelInfo, Role,
    provider::{
        ProviderDescriptor, ProviderEventStream, Response, provider_event_stream_from_response,
    },
};

/// Asserts an entry survived compaction unchanged in identity and content.
///
/// `parent_id` is deliberately excluded: it records where an entry sits on the
/// active path, and compaction genuinely moves salvaged entries. Holding the
/// old link would leave it pointing at an entry the replacement transcript no
/// longer contains. Identity (`id`), content, and `details` must not move.
fn assert_same_entry(actual: &TranscriptItem, expected: &TranscriptItem, context: &str) {
    assert_eq!(actual.id, expected.id, "{context} (identity)");
    assert_eq!(actual.kind, expected.kind, "{context} (kind)");
    assert_eq!(actual.message, expected.message, "{context} (message)");
    assert_eq!(actual.details(), expected.details(), "{context} (details)");
}

fn tool_exchange_item(text: &str) -> TranscriptItem {
    TranscriptItem::tool_exchange(
        Message::user(ContentBlock::text(text)),
        Some("tool_1".to_string()),
        false,
    )
}

fn user_turn_item(text: &str) -> TranscriptItem {
    TranscriptItem::user_turn(Message::user(ContentBlock::text(text)))
}

#[test]
fn local_summary_projection_excludes_thinking_from_full_transcript_json() {
    let items = vec![TranscriptItem::assistant_turn(Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Thinking {
                thinking: "private chain".to_string(),
                signature: Some("opaque-signature".to_string()),
                encrypted_content: None,
                id: None,
                provenance: Some(crate::ReasoningProvenance {
                    provider: crate::ProviderId::new("anthropic"),
                    model: "claude-test".to_string(),
                    format: crate::ReasoningFormat::AnthropicSigned,
                }),
                redacted: false,
            },
            ContentBlock::text("visible answer"),
        ],
    })];

    let serialized = serde_json::to_string(&items_without_thinking(&items)).unwrap();

    assert!(serialized.contains("visible answer"));
    assert!(!serialized.contains("private chain"));
    assert!(!serialized.contains("opaque-signature"));
    assert!(!serialized.contains("Thinking"));
    assert_eq!(items[0].message.as_ref().unwrap().content.len(), 2);
}

#[test]
fn extract_context_finds_file_paths_in_tool_exchanges() {
    let items = vec![
        tool_exchange_item("Reading file src/main.rs and also lib/utils.py"),
        tool_exchange_item("Modified path/to/config.toml successfully"),
    ];
    let ctx = extract_context(&items);
    assert!(
        ctx.files_touched.contains(&"src/main.rs".to_string()),
        "should find src/main.rs, got: {:?}",
        ctx.files_touched
    );
    assert!(
        ctx.files_touched.contains(&"lib/utils.py".to_string()),
        "should find lib/utils.py, got: {:?}",
        ctx.files_touched
    );
    assert!(
        ctx.files_touched
            .contains(&"path/to/config.toml".to_string()),
        "should find path/to/config.toml, got: {:?}",
        ctx.files_touched
    );
}

#[test]
fn extract_context_deduplicates_file_paths() {
    let items = vec![
        tool_exchange_item("Reading src/main.rs"),
        tool_exchange_item("Writing src/main.rs again"),
    ];
    let ctx = extract_context(&items);
    let count = ctx
        .files_touched
        .iter()
        .filter(|p| p.as_str() == "src/main.rs")
        .count();
    assert_eq!(count, 1, "file paths should be deduplicated");
}

#[test]
fn extract_context_ignores_file_paths_in_non_tool_items() {
    let items = vec![user_turn_item("Please edit src/main.rs")];
    let ctx = extract_context(&items);
    assert!(
        ctx.files_touched.is_empty(),
        "user turns should not contribute file paths, got: {:?}",
        ctx.files_touched
    );
}

#[test]
fn extract_context_finds_verification_outcomes() {
    let items = vec![
        tool_exchange_item("Running: cargo test result: ok. 5 passed; 0 FAILED"),
        tool_exchange_item("npm test completed with error code 1"),
    ];
    let ctx = extract_context(&items);
    assert!(
        !ctx.verification_outcomes.is_empty(),
        "should find verification outcomes"
    );
    assert!(
        ctx.verification_outcomes
            .iter()
            .any(|v| v.contains("cargo test") || v.contains("FAILED")),
        "should find cargo test outcome, got: {:?}",
        ctx.verification_outcomes
    );
}

#[test]
fn extract_context_finds_verification_in_any_item_kind() {
    let items = vec![user_turn_item("cargo test result: 10 passed; 0 FAILED")];
    let ctx = extract_context(&items);
    assert!(
        !ctx.verification_outcomes.is_empty(),
        "verification outcomes should be found in any item kind"
    );
}

#[test]
fn extract_context_finds_permission_decisions() {
    let items = vec![tool_exchange_item(
        "Permission denied for writing to /etc/hosts",
    )];
    let ctx = extract_context(&items);
    assert!(
        !ctx.permission_decisions.is_empty(),
        "should find permission decisions"
    );
}

#[test]
fn format_extracted_context_empty_produces_empty_string() {
    let ctx = ExtractedContext::default();
    let formatted = format_extracted_context(&ctx);
    assert!(formatted.is_empty());
}

#[test]
fn format_extracted_context_includes_all_sections() {
    let ctx = ExtractedContext {
        files_touched: vec!["src/main.rs".to_string()],
        verification_outcomes: vec!["cargo test passed".to_string()],
        permission_decisions: vec!["write permission denied".to_string()],
    };
    let formatted = format_extracted_context(&ctx);
    assert!(formatted.contains("FILES TOUCHED"));
    assert!(formatted.contains("src/main.rs"));
    assert!(formatted.contains("VERIFICATION OUTCOMES"));
    assert!(formatted.contains("cargo test passed"));
    assert!(formatted.contains("PERMISSION DECISIONS"));
    assert!(formatted.contains("write permission denied"));
}

#[test]
fn approx_token_count_uses_larger_of_two_heuristics() {
    // Short words: "a b c d" = 4 words * 1.3 = 5.2 -> 6, chars = 7 / 4 = 2
    assert!(approx_token_count("a b c d") >= 6);

    // Long word: "abcdefghijklmnop" = 1 word * 1.3 = 2, chars = 16 / 4 = 4
    assert!(approx_token_count("abcdefghijklmnop") >= 4);
}

#[test]
fn approx_token_count_empty_string() {
    assert_eq!(approx_token_count(""), 0);
}

#[test]
fn approx_token_count_items_sums_correctly() {
    let items = vec![
        user_turn_item("hello world"),
        tool_exchange_item("some tool output"),
    ];
    let total = approx_token_count_items(&items);
    let expected = approx_token_count("hello world") + approx_token_count("some tool output");
    assert_eq!(total, expected);
}

// -------------------------------------------------------------------
// M5: metadata-preserving compaction (mentra ADR-0001 §6)
// -------------------------------------------------------------------

fn delegation_result_item(label: &str) -> TranscriptItem {
    TranscriptItem::delegation_result(
        Message::user(ContentBlock::text(format!("{label} done"))),
        DelegationArtifact {
            kind: DelegationKind::Subagent,
            agent_id: format!("agent-{label}"),
            agent_name: label.to_string(),
            role: None,
            status: DelegationStatus::Finished,
            task_summary: format!("{label} task"),
            result_summary: None,
            artifacts: Vec::new(),
        },
        None,
    )
}

fn with_marker(item: TranscriptItem, key: &str, value: Value) -> TranscriptItem {
    item.with_details(BTreeMap::from([(key.to_string(), value)]))
}

// Regression test 1/2: proves `select_recent_user_turns` copies its
// selections verbatim rather than rebuilding them from `Message`. A
// regression that swapped `item.clone()` for something like
// `TranscriptItem::user_turn(item.message.clone().unwrap())` would
// produce items with `details: None` here, and the derived `PartialEq`
// (which compares every field, `details` included) would catch it.
#[test]
fn select_recent_user_turns_copies_items_verbatim_details_included() {
    let older = with_marker(user_turn_item("older"), "older", json!({ "keep": "older" }));
    let newer = with_marker(user_turn_item("newer"), "newer", json!({ "keep": "newer" }));
    let items = vec![
        older.clone(),
        tool_exchange_item("not a user turn"),
        newer.clone(),
    ];

    let selected = select_recent_user_turns(&items, 20_000);

    assert_eq!(selected, vec![older, newer]);
}

// Regression test 2/2: same property for
// `select_recent_delegation_results`.
#[test]
fn select_recent_delegation_results_copies_items_verbatim_details_included() {
    let first = with_marker(delegation_result_item("first"), "first", json!({ "n": 1 }));
    let second = with_marker(
        delegation_result_item("second"),
        "second",
        json!({ "n": 2 }),
    );
    let items = vec![
        first.clone(),
        user_turn_item("not a delegation result"),
        second.clone(),
    ];

    let selected = select_recent_delegation_results(&items, 8);

    assert_eq!(selected, vec![first, second]);
}

#[tokio::test]
async fn persist_transcript_snapshot_carries_every_items_details_bit_for_bit() {
    let items = vec![
        with_marker(user_turn_item("kept"), "kept", json!({ "n": 1 })),
        with_marker(
            tool_exchange_item("about to be discarded"),
            "about-to-be-discarded",
            json!({ "n": 2 }),
        ),
        TranscriptItem::assistant_turn(Message::assistant(ContentBlock::text("no details here"))),
    ];
    let dir = temp_dir("persist-transcript-details");

    let path = persist_transcript(&items, &dir)
        .await
        .expect("persist snapshot");

    let content = tokio::fs::read_to_string(&path)
        .await
        .expect("read snapshot");
    let reloaded: Vec<TranscriptItem> = content
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid TranscriptItem json"))
        .collect();
    assert_eq!(
        reloaded, items,
        "the pre-compaction snapshot must carry every item's details bit-for-bit, \
         including items about to be discarded by summarization"
    );
}

/// Minimal provider that returns one fixed local-summarization response —
/// enough to drive `StandardCompactionEngine::compact` end to end
/// without pulling in the full scripted-provider harness from
/// `agent::tests::support`, which is `pub(super)`-scoped to
/// `agent::tests` and unreachable from this module.
struct FixedSummaryProvider {
    model: ModelInfo,
}

#[async_trait]
impl Provider for FixedSummaryProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(self.model.provider.clone())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![self.model.clone()])
    }

    async fn stream(&self, _request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        Ok(provider_event_stream_from_response(Response {
            id: "fixed-summary-response".to_string(),
            model: self.model.id.clone(),
            role: Role::Assistant,
            content: vec![ContentBlock::text("test summary")],
            stop_reason: None,
            usage: None,
        }))
    }
}

#[tokio::test]
async fn compact_preserves_salvaged_details_and_lets_discarded_details_go_with_their_items() {
    let model = ModelInfo::new("test-model", "test-provider");
    let provider: Arc<dyn Provider> = Arc::new(FixedSummaryProvider {
        model: model.clone(),
    });

    // Compacted-away prefix: one user turn and one delegation result
    // that the engine salvages (and must copy verbatim, details
    // included), plus one assistant turn and one tool exchange that are
    // *not* salvaged and are honestly discarded along with their
    // details.
    let salvaged_user = with_marker(
        user_turn_item("first message"),
        "salvaged-user",
        json!({ "keep": "u0" }),
    );
    let discarded_assistant =
        TranscriptItem::assistant_turn(Message::assistant(ContentBlock::text("ack")));
    let salvaged_delegation = with_marker(
        delegation_result_item("helper"),
        "salvaged-delegation",
        json!({ "keep": "d0" }),
    );
    let discarded_tool_result = with_marker(
        tool_exchange_item("stale tool output"),
        "discarded-tool",
        json!({ "drop": "t0" }),
    );
    // Continuation tail: kept untouched outside the compacted prefix
    // (`required_tail_start_for_continuation` keeps the final
    // assistant tool_use + tool result pair intact).
    let tail_assistant =
        TranscriptItem::assistant_turn(Message::assistant(ContentBlock::ToolUse {
            id: "tail-1".to_string(),
            name: "tail_tool".to_string(),
            input: json!({}),
        }));
    let tail_result = with_marker(
        TranscriptItem::tool_exchange(
            Message::user(ContentBlock::text("tail tool output")),
            Some("tail-1".to_string()),
            false,
        ),
        "tail-tool",
        json!({ "keep": "tail" }),
    );

    let items = vec![
        salvaged_user.clone(),
        discarded_assistant.clone(),
        salvaged_delegation.clone(),
        discarded_tool_result.clone(),
        tail_assistant.clone(),
        tail_result.clone(),
    ];
    let transcript = AgentTranscript::new(items.clone());
    let transcript_dir = temp_dir("m5-compaction-salvage");

    let request = CompactionRequest {
        model: model.id.clone(),
        transcript,
        transcript_dir,
        summary_max_input_chars: 100_000,
        summary_max_output_tokens: 512,
        preserve_recent_user_tokens: 20_000,
        preserve_recent_delegation_results: 8,
        provider_request_options: ProviderRequestOptions::default(),
        mode: CompactionMode::LocalOnly,
        max_persisted_transcripts: None,
        instructions: None,
        bounds: CompactionBounds::default(),
    };

    let outcome = StandardCompactionEngine
        .compact(provider, request)
        .await
        .expect("compaction should not error")
        .expect("compaction should produce an outcome");

    // Counts stay consistent with the documented semantics: the whole
    // compacted prefix (4 items) counts as replaced even though two of
    // its items are also salvaged; only the untouched tail (2 items)
    // counts as preserved_items.
    assert_eq!(
        outcome.replaced_items, 4,
        "the whole compacted prefix counts as replaced, salvaged items included"
    );
    assert_eq!(
        outcome.preserved_items, 2,
        "preserved_items counts only the untouched continuation tail"
    );
    assert_eq!(outcome.preserved_user_turns, 1);
    assert_eq!(outcome.preserved_delegation_results, 1);

    let replacement = outcome.transcript.items();

    // Salvaged items survive verbatim, details included.
    let replayed_user = replacement
        .iter()
        .find(|item| item.is_real_user_turn())
        .expect("salvaged user turn present in the replacement transcript");
    assert_same_entry(
        replayed_user,
        &salvaged_user,
        "the salvaged user turn must survive bit-for-bit, details included",
    );

    let replayed_delegation = replacement
        .iter()
        .find(|item| item.is_delegation_result())
        .expect("salvaged delegation result present in the replacement transcript");
    assert_same_entry(
        replayed_delegation,
        &salvaged_delegation,
        "the salvaged delegation result must survive bit-for-bit, details included",
    );

    // The untouched tail survives verbatim too.
    assert_same_entry(
        replacement.last().expect("a tail item"),
        &tail_result,
        "the untouched tail item must survive bit-for-bit, details included",
    );

    // Discarded items are honestly gone: their details never resurface
    // on any other item in the replacement transcript.
    let replacement_json =
        serde_json::to_string(&replacement).expect("serialize replacement transcript");
    assert!(
        !replacement_json.contains("discarded-tool"),
        "a discarded item's details must not leak into the replacement transcript, got: {replacement_json}"
    );

    // But the pre-compaction snapshot on disk still has everything,
    // including the discarded item's details — the recovery artifact
    // for the summarized prefix.
    let snapshot = tokio::fs::read_to_string(&outcome.transcript_path)
        .await
        .expect("read pre-compaction snapshot");
    let snapshot_items: Vec<TranscriptItem> = snapshot
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid TranscriptItem json"))
        .collect();
    // Compared against the linked transcript rather than the raw vec the test
    // assembled: appending to a transcript is what establishes an entry's
    // parent, so only the linked form is what was ever snapshotted.
    let linked = AgentTranscript::new(items.clone());
    assert_eq!(
        snapshot_items,
        linked.items(),
        "the pre-compaction snapshot must preserve every original item bit-for-bit, \
         including ones the compaction goes on to discard"
    );
}

static NEXT_TEST_DIR_ID: AtomicU64 = AtomicU64::new(1);

fn temp_dir(label: &str) -> PathBuf {
    let unique = NEXT_TEST_DIR_ID.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mentra-compaction-test-{label}-{timestamp}-{unique}"
    ))
}

/// Builds a request around `items` with generous, non-interfering budgets.
fn request_for(items: Vec<TranscriptItem>, label: &str, model: &ModelInfo) -> CompactionRequest {
    CompactionRequest {
        model: model.id.clone(),
        transcript: AgentTranscript::new(items),
        transcript_dir: temp_dir(label),
        summary_max_input_chars: 100_000,
        summary_max_output_tokens: 512,
        preserve_recent_user_tokens: 20_000,
        preserve_recent_delegation_results: 8,
        provider_request_options: ProviderRequestOptions::default(),
        mode: CompactionMode::LocalOnly,
        max_persisted_transcripts: None,
        instructions: None,
        bounds: CompactionBounds::default(),
    }
}

fn fixed_provider(model: &ModelInfo) -> Arc<dyn Provider> {
    Arc::new(FixedSummaryProvider {
        model: model.clone(),
    })
}

#[tokio::test]
async fn a_turn_pinned_by_a_tool_result_is_summarized_instead_of_refused() {
    let model = ModelInfo::new("test-model", "test-provider");

    // The whole transcript is one assistant tool call and its result: the
    // continuation rule pins both, so there is nothing older to compact.
    // This used to return `Ok(None)`, leaving an over-budget turn stuck.
    let items = vec![
        TranscriptItem::assistant_turn(Message::assistant(ContentBlock::ToolUse {
            id: "only-1".to_string(),
            name: "huge_tool".to_string(),
            input: json!({}),
        })),
        TranscriptItem::tool_exchange(
            Message::user(ContentBlock::text("an enormous tool result")),
            Some("only-1".to_string()),
            false,
        ),
    ];

    let outcome = StandardCompactionEngine
        .compact(
            fixed_provider(&model),
            request_for(items, "m5-split-turn", &model),
        )
        .await
        .expect("compaction should not error")
        .expect("an over-budget turn must still compact");

    assert_eq!(outcome.replaced_items, 2, "both halves of the pair go");
    assert_eq!(outcome.preserved_items, 0, "nothing is pinned any more");

    // Crucially, the tool result is not left without its call.
    let kinds: Vec<&TranscriptKind> = outcome
        .transcript
        .items()
        .iter()
        .map(|item| &item.kind)
        .collect();
    assert!(
        !kinds
            .iter()
            .any(|kind| matches!(kind, TranscriptKind::ToolExchange { .. })),
        "a tool result must never survive without the call that produced it"
    );
    assert!(
        kinds
            .iter()
            .any(|kind| matches!(kind, TranscriptKind::CompactionSummary { .. })),
        "the pair is replaced by a summary"
    );
}

#[tokio::test]
async fn a_lone_user_turn_is_never_replaced_by_a_summary_of_itself() {
    let model = ModelInfo::new("test-model", "test-provider");
    let items = vec![user_turn_item("please do the thing")];

    let outcome = StandardCompactionEngine
        .compact(
            fixed_provider(&model),
            request_for(items, "m5-lone-user", &model),
        )
        .await
        .expect("compaction should not error");

    assert!(
        outcome.is_none(),
        "summarizing the user's only instruction would discard the very thing \
         the turn exists to convey"
    );
}

#[tokio::test]
async fn files_touched_accumulate_across_successive_compactions() {
    let model = ModelInfo::new("test-model", "test-provider");

    // A previous compaction already recorded a file that no surviving tool
    // exchange mentions any more.
    let earlier = CompactionSummary {
        files_touched: vec!["src/old.rs".to_string()],
        ..CompactionSummary::default()
    };

    let items = vec![
        TranscriptItem::compaction_summary(earlier),
        tool_exchange_item("edited src/new.rs just now"),
        TranscriptItem::assistant_turn(Message::assistant(ContentBlock::text("ack"))),
        user_turn_item("carry on"),
    ];

    let outcome = StandardCompactionEngine
        .compact(
            fixed_provider(&model),
            request_for(items, "m5-cumulative-files", &model),
        )
        .await
        .expect("compaction should not error")
        .expect("an outcome");

    assert!(
        outcome
            .summary
            .files_touched
            .contains(&"src/old.rs".to_string()),
        "a file recorded by an earlier compaction must survive the next one; \
         got {:?}",
        outcome.summary.files_touched
    );
    assert!(
        outcome
            .summary
            .files_touched
            .contains(&"src/new.rs".to_string()),
        "newly touched files must be added; got {:?}",
        outcome.summary.files_touched
    );
}

#[test]
fn accumulating_files_keeps_first_seen_order_and_drops_repeats() {
    let merged = accumulate_files(
        vec!["a.rs".to_string(), "b.rs".to_string()],
        ["b.rs", "c.rs"].into_iter(),
    );

    assert_eq!(merged, vec!["a.rs", "b.rs", "c.rs"]);
}

#[test]
fn carried_files_reads_the_newest_summary_only() {
    let older = CompactionSummary {
        files_touched: vec!["stale.rs".to_string()],
        ..CompactionSummary::default()
    };
    let newer = CompactionSummary {
        files_touched: vec!["fresh.rs".to_string()],
        ..CompactionSummary::default()
    };
    let items = vec![
        TranscriptItem::compaction_summary(older),
        TranscriptItem::compaction_summary(newer),
    ];

    // The newest summary is already cumulative, so reading only it is
    // sufficient — and reading every summary would resurrect files an
    // earlier round deliberately carried forward or dropped.
    assert_eq!(carried_files(&items), vec!["fresh.rs".to_string()]);
}

/// A provider whose summarization request is accepted and then never
/// answered, so a test can observe what a compaction does while a provider
/// call is genuinely in flight rather than simulating one.
struct BlockingProvider {
    model: ModelInfo,
    requests: Arc<AtomicU64>,
}

#[async_trait]
impl Provider for BlockingProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(self.model.provider.clone())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![self.model.clone()])
    }

    async fn stream(&self, _request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
}

fn compactable_items() -> Vec<TranscriptItem> {
    vec![
        user_turn_item("please do the thing"),
        TranscriptItem::assistant_turn(Message::assistant(ContentBlock::text("ack"))),
        tool_exchange_item("some tool output"),
        user_turn_item("carry on"),
    ]
}

// Virtual time: the provider never answers, so what is being measured is how
// long the compaction waits before giving up on it. Under `start_paused` the
// clock advances only when the runtime is idle, so the elapsed value is the
// bound checker's own sleeps rather than the test machine's load.
#[tokio::test(start_paused = true)]
async fn a_cancelled_compaction_gives_up_without_waiting_for_the_provider() {
    let model = ModelInfo::new("test-model", "test-provider");
    let requests = Arc::new(AtomicU64::new(0));
    let provider: Arc<dyn Provider> = Arc::new(BlockingProvider {
        model: model.clone(),
        requests: Arc::clone(&requests),
    });
    let cancellation = crate::runtime::CancellationToken::default();

    let mut request = request_for(compactable_items(), "bounds-cancelled", &model);
    request.bounds = CompactionBounds {
        cancellation: Some(cancellation.clone()),
        deadline: None,
    };

    let canceller = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancellation.cancel();
    });

    let started = tokio::time::Instant::now();
    let result = StandardCompactionEngine.compact(provider, request).await;
    canceller.await.expect("canceller task");

    assert!(
        matches!(result, Err(RuntimeError::Cancelled)),
        "a cancelled compaction reports cancellation, not a compaction failure; got {result:?}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_millis(150),
        "the compaction must abandon the in-flight call within a poll interval \
         of the cancel, not wait for an answer that never comes"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "the provider was reached and then abandoned mid-call"
    );
}

#[tokio::test]
async fn an_expired_deadline_stops_a_compaction_before_it_reaches_the_provider() {
    let model = ModelInfo::new("test-model", "test-provider");
    let requests = Arc::new(AtomicU64::new(0));
    let provider: Arc<dyn Provider> = Arc::new(BlockingProvider {
        model: model.clone(),
        requests: Arc::clone(&requests),
    });

    let mut request = request_for(compactable_items(), "bounds-deadline", &model);
    let transcript_dir = request.transcript_dir.clone();
    request.bounds = CompactionBounds {
        cancellation: None,
        deadline: Some(SystemTime::now() - std::time::Duration::from_secs(1)),
    };

    let result = StandardCompactionEngine.compact(provider, request).await;

    assert!(
        matches!(result, Err(RuntimeError::DeadlineExceeded)),
        "got {result:?}"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "a compaction past its deadline must not spend a provider request"
    );
    assert!(
        !transcript_dir.exists(),
        "nor write a pre-compaction snapshot for work it will not do"
    );
}

#[tokio::test]
async fn unbounded_compaction_is_unchanged() {
    // The bounds are additive: the default is no bound, and a request that
    // carries none behaves exactly as it did before they existed.
    let model = ModelInfo::new("test-model", "test-provider");
    let request = request_for(compactable_items(), "bounds-absent", &model);

    assert!(request.bounds.cancellation.is_none());
    assert!(request.bounds.deadline.is_none());

    let outcome = StandardCompactionEngine
        .compact(fixed_provider(&model), request)
        .await
        .expect("an unbounded compaction should not error")
        .expect("an outcome");

    assert!(outcome.replaced_items > 0);
}
