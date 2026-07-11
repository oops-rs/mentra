use std::collections::HashMap;

use crate::{
    memory::{MemoryCursor, MemoryRecord, MemorySearchRequest, MemoryStore},
    runtime::RuntimeError,
};

use super::VolatileRuntimeStore;

/// Long-term memory records and per-agent ingest cursors, mirroring the
/// default store's `long_term_memory` / `long_term_memory_cursor` tables.
///
/// Search here is a simple case-insensitive substring match over each query
/// token, not the default store's BM25-ranked full-text search — the
/// volatile profile favors simplicity over search quality for ephemeral
/// runs.
#[derive(Default)]
pub(super) struct MemoryState {
    records: HashMap<String, MemoryRecord>,
    cursors: HashMap<String, MemoryCursor>,
}

fn query_tokens(query: &str) -> Vec<String> {
    query
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect()
}

impl MemoryStore for VolatileRuntimeStore {
    fn upsert_records(&self, records: &[MemoryRecord]) -> Result<(), RuntimeError> {
        let mut state = self.lock();
        for record in records {
            state
                .memory
                .records
                .insert(record.record_id.clone(), record.clone());
        }
        Ok(())
    }

    fn search_records_with_options(
        &self,
        request: &MemorySearchRequest,
    ) -> Result<Vec<MemoryRecord>, RuntimeError> {
        if request.limit == 0 {
            return Ok(Vec::new());
        }
        let tokens = query_tokens(&request.query);
        if tokens.is_empty() {
            return Ok(Vec::new());
        }

        let state = self.lock();
        let mut matches: Vec<MemoryRecord> = state
            .memory
            .records
            .values()
            .filter(|record| {
                if record.agent_id != request.agent_id {
                    return false;
                }
                let content = record.content.to_lowercase();
                tokens.iter().any(|token| content.contains(token))
            })
            .cloned()
            .collect();
        matches.sort_by_key(|record| std::cmp::Reverse(record.created_at));
        matches.truncate(request.limit);
        Ok(matches)
    }

    fn delete_records(&self, record_ids: &[String]) -> Result<(), RuntimeError> {
        let mut state = self.lock();
        for id in record_ids {
            state.memory.records.remove(id);
        }
        Ok(())
    }

    fn tombstone_records(
        &self,
        agent_id: &str,
        record_ids: &[String],
    ) -> Result<usize, RuntimeError> {
        let mut state = self.lock();
        let mut affected = 0usize;
        for id in record_ids {
            let matches_agent = state
                .memory
                .records
                .get(id)
                .is_some_and(|record| record.agent_id == agent_id);
            if matches_agent {
                state.memory.records.remove(id);
                affected += 1;
            }
        }
        Ok(affected)
    }

    fn load_agent_memory_cursor(
        &self,
        agent_id: &str,
    ) -> Result<Option<MemoryCursor>, RuntimeError> {
        Ok(self.lock().memory.cursors.get(agent_id).cloned())
    }

    fn save_agent_memory_cursor(
        &self,
        agent_id: &str,
        cursor: &MemoryCursor,
    ) -> Result<(), RuntimeError> {
        self.lock()
            .memory
            .cursors
            .insert(agent_id.to_string(), cursor.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::memory::{MemoryCursor, MemoryRecord, MemoryRecordKind, MemoryStore};

    use super::super::VolatileRuntimeStore;

    fn record(id: &str, agent_id: &str, content: &str, created_at: i64) -> MemoryRecord {
        MemoryRecord {
            record_id: id.to_string(),
            agent_id: agent_id.to_string(),
            kind: MemoryRecordKind::Episode,
            content: content.to_string(),
            source_revision: 1,
            created_at,
            metadata_json: "{}".to_string(),
            source: None,
            pinned: false,
            score: None,
        }
    }

    #[test]
    fn search_is_scoped_to_the_requesting_agent() {
        let store = VolatileRuntimeStore::new();
        store
            .upsert_records(&[
                record("episode:a:1", "agent-a", "shared phrase alpha", 1),
                record("episode:b:1", "agent-b", "shared phrase alpha", 2),
            ])
            .expect("seed records");

        let hits = store
            .search_records("agent-a", "alpha", 10)
            .expect("search agent-a");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record_id, "episode:a:1");
    }

    #[test]
    fn search_ignores_non_searchable_queries() {
        let store = VolatileRuntimeStore::new();
        store
            .upsert_records(&[record("episode:a:1", "agent-a", "alpha", 1)])
            .expect("seed records");

        assert!(
            store
                .search_records("agent-a", "... ---", 10)
                .expect("search punctuation-only query")
                .is_empty()
        );
    }

    #[test]
    fn tombstone_only_removes_records_owned_by_the_agent() {
        let store = VolatileRuntimeStore::new();
        store
            .upsert_records(&[record("episode:a:1", "agent-a", "alpha", 1)])
            .expect("seed records");

        let affected = store
            .tombstone_records("agent-b", &["episode:a:1".to_string()])
            .expect("tombstone with wrong owner");
        assert_eq!(affected, 0);

        let affected = store
            .tombstone_records("agent-a", &["episode:a:1".to_string()])
            .expect("tombstone with correct owner");
        assert_eq!(affected, 1);
    }

    #[test]
    fn memory_cursor_round_trips() {
        let store = VolatileRuntimeStore::new();
        assert_eq!(
            store
                .load_agent_memory_cursor("agent-a")
                .expect("load absent cursor"),
            None
        );

        let cursor = MemoryCursor {
            last_ingested_revision: 7,
        };
        store
            .save_agent_memory_cursor("agent-a", &cursor)
            .expect("save cursor");
        assert_eq!(
            store
                .load_agent_memory_cursor("agent-a")
                .expect("load saved cursor"),
            Some(cursor)
        );
    }
}
