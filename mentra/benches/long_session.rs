use criterion::{Criterion, criterion_group, criterion_main};
use mentra::{
    ContentBlock,
    memory::{MemoryRecord, MemoryRecordKind, MemorySearchRequest, MemorySearchMode, MemoryStore},
    runtime::SqliteRuntimeStore,
    test::{MockRuntimeBuilder, MockTurn},
};
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn temp_sqlite_path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "mentra-bench-{}-{}.sqlite",
        label,
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ))
}

fn build_mock_runtime(
    n_turns: usize,
    store_path: std::path::PathBuf,
) -> (mentra::test::MockRuntime, mentra::ModelInfo) {
    let store = SqliteRuntimeStore::new(store_path);
    let mut builder = MockRuntimeBuilder::default().with_store(store);
    for i in 0..n_turns {
        builder = builder.push_turn(MockTurn::Text(format!("turn {i} response")));
    }
    let mock = builder.build().expect("build mock runtime");
    let model = mock.model();
    (mock, model)
}

// ---------------------------------------------------------------------------
// bench_500_turn_session
// Measures total wall time for driving 500 send/response cycles.
// ---------------------------------------------------------------------------

fn bench_500_turn_session(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

    c.bench_function("500_turn_session", |b| {
        b.to_async(&rt).iter(|| async {
            let store_path = temp_sqlite_path("500turns");
            let (mock, model) = build_mock_runtime(500, store_path);
            let mut agent = mock
                .runtime()
                .spawn("bench-agent", model)
                .expect("spawn agent");

            for i in 0u32..500 {
                agent
                    .send(vec![ContentBlock::text(format!("message {i}"))])
                    .await
                    .expect("send turn");
            }
        });
    });
}

// ---------------------------------------------------------------------------
// bench_resume_after_heavy_session
// 200 turns followed by a resume from the persisted agent record.
// ---------------------------------------------------------------------------

fn bench_resume_after_heavy_session(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

    c.bench_function("resume_after_200_turn_session", |b| {
        b.to_async(&rt).iter(|| async {
            let store_path = temp_sqlite_path("resume");
            // Prepare: 200 turns for the heavy session + 1 turn for the resumed send
            let store = SqliteRuntimeStore::new(store_path.clone());
            let mut builder = MockRuntimeBuilder::default().with_store(store);
            for i in 0..201usize {
                builder = builder.push_turn(MockTurn::Text(format!("turn {i} response")));
            }
            let mock = builder.build().expect("build mock runtime");
            let model = mock.model();

            // Drive the heavy session
            let agent_id = {
                let mut agent = mock
                    .runtime()
                    .spawn("bench-agent", model.clone())
                    .expect("spawn agent");
                for i in 0u32..200 {
                    agent
                        .send(vec![ContentBlock::text(format!("message {i}"))])
                        .await
                        .expect("send turn");
                }
                agent.id().to_string()
            };

            // Measure: resume and send one more turn
            let mut resumed = mock
                .runtime()
                .resume_agent(&agent_id)
                .expect("resume agent");
            resumed
                .send(vec![ContentBlock::text("resumed message")])
                .await
                .expect("send after resume");
        });
    });
}

// ---------------------------------------------------------------------------
// bench_memory_scaling_1000_records
// Seeds 1000 memory records into a SQLite store and measures search latency.
// ---------------------------------------------------------------------------

fn bench_memory_scaling_1000_records(c: &mut Criterion) {
    use mentra::memory::SqliteHybridMemoryStore;

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

    c.bench_function("memory_search_1000_records", |b| {
        b.to_async(&rt).iter(|| async {
            let store_path = temp_sqlite_path("memory");
            let store = SqliteHybridMemoryStore::new(&store_path);

            // Seed 1000 records
            let records: Vec<MemoryRecord> = (0..1000usize)
                .map(|i| MemoryRecord {
                    record_id: format!("rec-{i:04}"),
                    agent_id: "bench-agent".to_string(),
                    kind: if i % 3 == 0 {
                        MemoryRecordKind::Episode
                    } else if i % 3 == 1 {
                        MemoryRecordKind::Fact
                    } else {
                        MemoryRecordKind::Summary
                    },
                    content: format!(
                        "memory record {i}: the agent discussed topic {i} in session {i}"
                    ),
                    source_revision: i as u64,
                    created_at: now_secs() - i as i64,
                    metadata_json: "{}".to_string(),
                    source: None,
                    pinned: false,
                    score: None,
                })
                .collect();

            store.upsert_records(&records).expect("seed records");

            // Measure search latency
            let request = MemorySearchRequest {
                agent_id: "bench-agent".to_string(),
                query: "agent discussed topic session".to_string(),
                limit: 20,
                char_budget: None,
                mode: MemorySearchMode::Automatic,
            };
            let _hits = store
                .search_records_with_options(&request)
                .expect("search records");
        });
    });
}

// ---------------------------------------------------------------------------
// Criterion group
// ---------------------------------------------------------------------------

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10);
    targets = bench_500_turn_session, bench_resume_after_heavy_session, bench_memory_scaling_1000_records
}
criterion_main!(benches);
