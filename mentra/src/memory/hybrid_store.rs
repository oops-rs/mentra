use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::{
    memory::{MemoryCursor, MemoryRecord, MemoryRecordKind, MemorySearchRequest, MemoryStore},
    runtime::RuntimeError,
};

#[derive(Clone)]
/// SQLite-backed hybrid memory store with provenance, pinning, and tombstoning support.
pub struct SqliteHybridMemoryStore {
    path: PathBuf,
}

impl SqliteHybridMemoryStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        self.path.as_path()
    }

    fn open(&self) -> Result<Connection, RuntimeError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| RuntimeError::Store(error.to_string()))?;
        }
        let conn = Connection::open(&self.path).map_err(sqlite_error)?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(sqlite_error)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(sqlite_error)?;
        self.ensure_schema(&conn)?;
        Ok(conn)
    }

    fn ensure_schema(&self, conn: &Connection) -> Result<(), RuntimeError> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS memory_records (
                record_id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                content TEXT NOT NULL,
                source_revision INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                metadata_json TEXT NOT NULL,
                source_json TEXT,
                pinned INTEGER NOT NULL DEFAULT 0,
                tombstoned_at INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_memory_records_agent_created
                ON memory_records (agent_id, created_at DESC);
            CREATE VIRTUAL TABLE IF NOT EXISTS memory_records_fts USING fts5(
                record_id UNINDEXED,
                agent_id UNINDEXED,
                content
            );
            CREATE TABLE IF NOT EXISTS memory_cursor (
                agent_id TEXT PRIMARY KEY,
                cursor_json TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );
            "#,
        )
        .map_err(sqlite_error)
    }

    fn search_records_raw(
        &self,
        request: &MemorySearchRequest,
    ) -> Result<Vec<MemoryRecord>, RuntimeError> {
        if request.query.trim().is_empty() || request.limit == 0 {
            return Ok(Vec::new());
        }
        let Some(query) = fts_query(&request.query) else {
            return Ok(Vec::new());
        };

        let conn = self.open()?;
        let mut stmt = conn
            .prepare(
                r#"
                SELECT
                    record.record_id,
                    record.agent_id,
                    record.kind,
                    record.content,
                    record.source_revision,
                    record.created_at,
                    record.metadata_json,
                    record.source_json,
                    record.pinned,
                    bm25(memory_records_fts) AS rank
                FROM memory_records_fts
                JOIN memory_records AS record ON record.record_id = memory_records_fts.record_id
                WHERE memory_records_fts.agent_id = ?1
                  AND memory_records_fts.content MATCH ?2
                  AND record.tombstoned_at IS NULL
                LIMIT ?3
                "#,
            )
            .map_err(sqlite_error)?;

        let candidate_limit = request.limit.saturating_mul(5).clamp(10, 50) as i64;
        let mut records = stmt
            .query_map(params![request.agent_id, query, candidate_limit], |row| {
                let kind = row.get::<_, String>(2)?;
                let source_json = row.get::<_, Option<String>>(7)?;
                let pinned = row.get::<_, i64>(8)? != 0;
                let raw_rank = row.get::<_, Option<f64>>(9)?.unwrap_or(0.0);
                let created_at = row.get::<_, i64>(5)?;
                let score = rank_score(parse_memory_kind(&kind), pinned, created_at, raw_rank);
                Ok(MemoryRecord {
                    record_id: row.get(0)?,
                    agent_id: row.get(1)?,
                    kind: parse_memory_kind(&kind),
                    content: row.get(3)?,
                    source_revision: row.get::<_, i64>(4)? as u64,
                    created_at,
                    metadata_json: row.get(6)?,
                    source: decode_source(source_json),
                    pinned,
                    score: Some(score),
                })
            })
            .map_err(sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sqlite_error)?;

        records.sort_by(|left, right| {
            right
                .score
                .partial_cmp(&left.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| right.created_at.cmp(&left.created_at))
        });
        records.truncate(request.limit);
        Ok(records)
    }
}

impl MemoryStore for SqliteHybridMemoryStore {
    fn upsert_records(&self, records: &[MemoryRecord]) -> Result<(), RuntimeError> {
        if records.is_empty() {
            return Ok(());
        }

        let mut conn = self.open()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        let now = now_secs();

        for record in records {
            tx.execute(
                r#"
                INSERT INTO memory_records (
                    record_id, agent_id, kind, content, source_revision, created_at, updated_at,
                    metadata_json, source_json, pinned, tombstoned_at
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL)
                ON CONFLICT(record_id) DO UPDATE SET
                    agent_id = excluded.agent_id,
                    kind = excluded.kind,
                    content = excluded.content,
                    source_revision = excluded.source_revision,
                    created_at = excluded.created_at,
                    updated_at = excluded.updated_at,
                    metadata_json = excluded.metadata_json,
                    source_json = excluded.source_json,
                    pinned = excluded.pinned,
                    tombstoned_at = NULL
                "#,
                params![
                    record.record_id,
                    record.agent_id,
                    kind_name(record.kind),
                    record.content,
                    record.source_revision as i64,
                    record.created_at,
                    now,
                    record.metadata_json,
                    encode_source(record.source.as_deref())?,
                    if record.pinned { 1 } else { 0 },
                ],
            )
            .map_err(sqlite_error)?;
            tx.execute(
                "DELETE FROM memory_records_fts WHERE record_id = ?1",
                params![record.record_id],
            )
            .map_err(sqlite_error)?;
            tx.execute(
                "INSERT INTO memory_records_fts (record_id, agent_id, content) VALUES (?1, ?2, ?3)",
                params![record.record_id, record.agent_id, record.content],
            )
            .map_err(sqlite_error)?;
        }

        tx.commit().map_err(sqlite_error)
    }

    fn search_records_with_options(
        &self,
        request: &MemorySearchRequest,
    ) -> Result<Vec<MemoryRecord>, RuntimeError> {
        self.search_records_raw(request)
    }

    fn delete_records(&self, record_ids: &[String]) -> Result<(), RuntimeError> {
        if record_ids.is_empty() {
            return Ok(());
        }

        let mut conn = self.open()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        for record_id in record_ids {
            tx.execute(
                "DELETE FROM memory_records_fts WHERE record_id = ?1",
                params![record_id],
            )
            .map_err(sqlite_error)?;
            tx.execute(
                "DELETE FROM memory_records WHERE record_id = ?1",
                params![record_id],
            )
            .map_err(sqlite_error)?;
        }
        tx.commit().map_err(sqlite_error)
    }

    fn tombstone_records(
        &self,
        agent_id: &str,
        record_ids: &[String],
    ) -> Result<usize, RuntimeError> {
        if record_ids.is_empty() {
            return Ok(0);
        }

        let mut conn = self.open()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        let mut affected = 0usize;
        let now = now_secs();

        for record_id in record_ids {
            let updated = tx
                .execute(
                    r#"
                    UPDATE memory_records
                    SET tombstoned_at = ?3, updated_at = ?3
                    WHERE record_id = ?1 AND agent_id = ?2 AND tombstoned_at IS NULL
                    "#,
                    params![record_id, agent_id, now],
                )
                .map_err(sqlite_error)?;
            if updated > 0 {
                affected += updated;
                tx.execute(
                    "DELETE FROM memory_records_fts WHERE record_id = ?1",
                    params![record_id],
                )
                .map_err(sqlite_error)?;
            }
        }

        tx.commit().map_err(sqlite_error)?;
        Ok(affected)
    }

    fn load_agent_memory_cursor(
        &self,
        agent_id: &str,
    ) -> Result<Option<MemoryCursor>, RuntimeError> {
        let conn = self.open()?;
        conn.query_row(
            "SELECT cursor_json FROM memory_cursor WHERE agent_id = ?1",
            params![agent_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sqlite_error)?
        .map(|json| from_json(&json))
        .transpose()
    }

    fn save_agent_memory_cursor(
        &self,
        agent_id: &str,
        cursor: &MemoryCursor,
    ) -> Result<(), RuntimeError> {
        let conn = self.open()?;
        conn.execute(
            r#"
            INSERT INTO memory_cursor (agent_id, cursor_json, updated_at)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(agent_id) DO UPDATE SET
                cursor_json = excluded.cursor_json,
                updated_at = excluded.updated_at
            "#,
            params![agent_id, to_json(cursor)?, now_secs()],
        )
        .map_err(sqlite_error)?;
        Ok(())
    }
}

fn parse_memory_kind(kind: &str) -> MemoryRecordKind {
    match kind {
        "summary" => MemoryRecordKind::Summary,
        "fact" => MemoryRecordKind::Fact,
        _ => MemoryRecordKind::Episode,
    }
}

fn kind_name(kind: MemoryRecordKind) -> &'static str {
    match kind {
        MemoryRecordKind::Episode => "episode",
        MemoryRecordKind::Summary => "summary",
        MemoryRecordKind::Fact => "fact",
    }
}

fn rank_score(kind: MemoryRecordKind, pinned: bool, created_at: i64, raw_rank: f64) -> f64 {
    let kind_bonus = match kind {
        MemoryRecordKind::Fact => 3.0,
        MemoryRecordKind::Summary => 1.5,
        MemoryRecordKind::Episode => 0.0,
    };
    let manual_bonus = if pinned { 2.0 } else { 0.0 };
    let age_hours = ((now_secs() - created_at).max(0) as f64) / 3600.0;
    let recency_bonus = 0.5 / (1.0 + age_hours / 24.0);
    let text_bonus = 8.0 / (1.0 + raw_rank.abs());
    text_bonus + kind_bonus + manual_bonus + recency_bonus
}

fn encode_source(source: Option<&str>) -> Result<Option<String>, RuntimeError> {
    source
        .map(|value| {
            serde_json::to_string(value).map_err(|error| RuntimeError::Store(error.to_string()))
        })
        .transpose()
}

fn decode_source(source_json: Option<String>) -> Option<String> {
    source_json.and_then(|json| serde_json::from_str::<String>(&json).ok())
}

fn to_json<T: serde::Serialize>(value: &T) -> Result<String, RuntimeError> {
    serde_json::to_string(value).map_err(|error| RuntimeError::Store(error.to_string()))
}

fn from_json<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, RuntimeError> {
    serde_json::from_str(value).map_err(|error| RuntimeError::Store(error.to_string()))
}

fn sqlite_error(error: rusqlite::Error) -> RuntimeError {
    RuntimeError::Store(error.to_string())
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn fts_query(query: &str) -> Option<String> {
    let tokens = query
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(|token| format!("\"{token}\""))
        .collect::<Vec<_>>();

    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" OR "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryStore;

    #[test]
    fn pinned_manual_facts_outrank_episodes() {
        let store = SqliteHybridMemoryStore::new(
            std::env::temp_dir().join(format!("mentra-hybrid-memory-{}.sqlite", now_secs())),
        );
        store
            .upsert_records(&[
                MemoryRecord {
                    record_id: "episode:1".to_string(),
                    agent_id: "agent-1".to_string(),
                    kind: MemoryRecordKind::Episode,
                    content: "shared phrase alpha".to_string(),
                    source_revision: 1,
                    created_at: now_secs(),
                    metadata_json: "{}".to_string(),
                    source: Some("auto_ingest".to_string()),
                    pinned: false,
                    score: None,
                },
                MemoryRecord {
                    record_id: "fact:1".to_string(),
                    agent_id: "agent-1".to_string(),
                    kind: MemoryRecordKind::Fact,
                    content: "shared phrase alpha".to_string(),
                    source_revision: 2,
                    created_at: now_secs(),
                    metadata_json: "{}".to_string(),
                    source: Some("manual_pin".to_string()),
                    pinned: true,
                    score: None,
                },
            ])
            .expect("seed records");

        let records = store
            .search_records_with_options(&MemorySearchRequest {
                agent_id: "agent-1".to_string(),
                query: "shared alpha".to_string(),
                limit: 2,
                char_budget: None,
            })
            .expect("search");
        assert_eq!(records[0].record_id, "fact:1");
    }

    #[test]
    fn tombstoned_records_are_excluded_from_reads() {
        let store = SqliteHybridMemoryStore::new(
            std::env::temp_dir().join(format!("mentra-hybrid-tombstone-{}.sqlite", now_secs())),
        );
        store
            .upsert_records(&[MemoryRecord {
                record_id: "fact:1".to_string(),
                agent_id: "agent-1".to_string(),
                kind: MemoryRecordKind::Fact,
                content: "preferred editor is vim".to_string(),
                source_revision: 1,
                created_at: now_secs(),
                metadata_json: "{}".to_string(),
                source: Some("manual_pin".to_string()),
                pinned: true,
                score: None,
            }])
            .expect("seed records");
        assert_eq!(
            store
                .tombstone_records("agent-1", &["fact:1".to_string()])
                .expect("tombstone"),
            1
        );

        let records = store.search_records("agent-1", "vim", 5).expect("search");
        assert!(records.is_empty());
    }

    #[test]
    fn punctuation_heavy_queries_still_return_results() {
        let store = SqliteHybridMemoryStore::new(
            std::env::temp_dir().join(format!("mentra-hybrid-punct-{}.sqlite", now_secs())),
        );
        store
            .upsert_records(&[MemoryRecord {
                record_id: "episode:1".to_string(),
                agent_id: "agent-1".to_string(),
                kind: MemoryRecordKind::Episode,
                content: "shared phrase alpha".to_string(),
                source_revision: 1,
                created_at: now_secs(),
                metadata_json: "{}".to_string(),
                source: Some("auto_ingest".to_string()),
                pinned: false,
                score: None,
            }])
            .expect("seed records");

        let records = store
            .search_records("agent-1", "(shared) alpha!!!", 5)
            .expect("search");
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn compatibility_search_wrapper_matches_options_search() {
        let store = SqliteHybridMemoryStore::new(
            std::env::temp_dir().join(format!("mentra-hybrid-compat-{}.sqlite", now_secs())),
        );
        store
            .upsert_records(&[MemoryRecord {
                record_id: "episode:1".to_string(),
                agent_id: "agent-1".to_string(),
                kind: MemoryRecordKind::Episode,
                content: "shared phrase alpha".to_string(),
                source_revision: 1,
                created_at: now_secs(),
                metadata_json: "{}".to_string(),
                source: Some("auto_ingest".to_string()),
                pinned: false,
                score: None,
            }])
            .expect("seed records");

        let compat = store
            .search_records("agent-1", "shared alpha", 5)
            .expect("compat search");
        let explicit = store
            .search_records_with_options(&MemorySearchRequest {
                agent_id: "agent-1".to_string(),
                query: "shared alpha".to_string(),
                limit: 5,
                char_budget: None,
            })
            .expect("explicit search");
        assert_eq!(compat.len(), explicit.len());
        for (compat_record, explicit_record) in compat.iter().zip(explicit.iter()) {
            assert_eq!(compat_record.record_id, explicit_record.record_id);
            assert_eq!(compat_record.agent_id, explicit_record.agent_id);
            assert_eq!(compat_record.kind, explicit_record.kind);
            assert_eq!(compat_record.content, explicit_record.content);
            assert_eq!(
                compat_record.source_revision,
                explicit_record.source_revision
            );
            assert_eq!(compat_record.created_at, explicit_record.created_at);
            assert_eq!(compat_record.metadata_json, explicit_record.metadata_json);
            assert_eq!(compat_record.source, explicit_record.source);
            assert_eq!(compat_record.pinned, explicit_record.pinned);

            let compat_score = compat_record.score.expect("compat score");
            let explicit_score = explicit_record.score.expect("explicit score");
            assert!(
                (compat_score - explicit_score).abs() < 1e-5,
                "expected comparable ranking scores, got {compat_score} vs {explicit_score}"
            );
        }
    }
}
