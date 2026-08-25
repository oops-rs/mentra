//! The append-only transcript entry log (`transcript.jsonl`).
//!
//! Each line is one [`TranscriptItem`] with a `schema` field added, keyed by
//! its entry id. The log is history: entries are appended when first seen
//! and never rewritten, so abandoned branches and entries superseded by
//! compaction or a run rollback remain greppable. On replay the last
//! occurrence of an id wins; a truncated final line — the only damage a
//! crashed append can leave — is skipped.

use std::{
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
    path::Path,
};

use serde::{Deserialize, Serialize};

use crate::{runtime::error::RuntimeError, transcript::TranscriptItem};

use super::{SCHEMA_VERSION, fs_util, store_error};

/// One log line on its way in or out: the entry's own fields flattened
/// beside the layout revision that wrote it.
#[derive(Deserialize)]
struct LogLine {
    schema: u32,
    #[serde(flatten)]
    item: TranscriptItem,
}

#[derive(Serialize)]
struct LogLineRef<'a> {
    schema: u32,
    #[serde(flatten)]
    item: &'a TranscriptItem,
}

/// What the store remembers about one agent's on-disk log: which entry ids
/// it already holds. Debug builds also keep a content hash per id, to catch
/// the day an entry stops being immutable.
#[derive(Default)]
pub(super) struct TranscriptLogIndex {
    loaded: bool,
    entries: HashMap<String, u64>,
}

impl TranscriptLogIndex {
    /// Loads the index from the log file on first use.
    fn ensure_loaded(&mut self, path: &Path) -> Result<(), RuntimeError> {
        if self.loaded {
            return Ok(());
        }
        self.entries = read_log(path)?
            .into_iter()
            .map(|(id, item)| Ok((id, verification_hash(&item)?)))
            .collect::<Result<_, RuntimeError>>()?;
        self.loaded = true;
        Ok(())
    }

    /// Appends every entry the log does not already hold, in the order
    /// given, fsyncing once. Membership by id decides: entry content is
    /// immutable once appended — nothing in the runtime rewrites an existing
    /// [`TranscriptItem`] — so an already-logged id costs no serialization.
    /// Debug builds verify that immutability instead of assuming it.
    pub(super) fn append_missing<'a>(
        &mut self,
        path: &Path,
        entries: impl Iterator<Item = &'a TranscriptItem>,
    ) -> Result<(), RuntimeError> {
        self.ensure_loaded(path)?;
        let mut lines = Vec::new();
        let mut pending: Vec<(String, u64)> = Vec::new();
        let mut pending_ids: HashSet<&str> = HashSet::new();
        for item in entries {
            let id = item.id.as_str();
            if pending_ids.contains(id) {
                continue;
            }
            if let Some(&logged) = self.entries.get(id) {
                #[cfg(debug_assertions)]
                {
                    let current = verification_hash(item)?;
                    debug_assert_eq!(
                        logged, current,
                        "transcript entry '{id}' changed after being logged"
                    );
                }
                #[cfg(not(debug_assertions))]
                let _ = logged;
                continue;
            }
            lines.push(log_line(item)?);
            pending.push((id.to_string(), verification_hash(item)?));
            pending_ids.insert(id);
        }
        drop(pending_ids);
        fs_util::append_lines(path, &lines)?;
        self.entries.extend(pending);
        Ok(())
    }
}

/// Reads the log into id → entry, last occurrence winning. A missing file is
/// an empty log. A final line that does not parse is the truncated remains
/// of a crashed append and is skipped; damage anywhere else is corruption
/// and is reported.
pub(super) fn read_log(path: &Path) -> Result<HashMap<String, TranscriptItem>, RuntimeError> {
    let Some(contents) = fs_util::read_optional(path)? else {
        return Ok(HashMap::new());
    };

    let mut entries = HashMap::new();
    let lines: Vec<&str> = contents.split('\n').collect();
    let last_index = lines.len().saturating_sub(1);
    for (index, line) in lines.iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        match parse_line(line) {
            Ok(item) => {
                entries.insert(item.id.as_str().to_string(), item);
            }
            Err(error) => {
                let final_line = index == last_index && !contents.ends_with('\n');
                if final_line {
                    // The truncated tail of an append cut short by a crash.
                    continue;
                }
                return Err(store_error(
                    &format!("parse '{}' line {}", path.display(), index + 1),
                    error,
                ));
            }
        }
    }
    Ok(entries)
}

fn parse_line(line: &str) -> Result<TranscriptItem, RuntimeError> {
    let parsed: LogLine =
        serde_json::from_str(line).map_err(|error| RuntimeError::Store(error.to_string()))?;
    if parsed.schema > SCHEMA_VERSION {
        return Err(RuntimeError::Store(format!(
            "transcript line schema {} is newer than this build understands ({SCHEMA_VERSION})",
            parsed.schema
        )));
    }
    Ok(parsed.item)
}

fn log_line(item: &TranscriptItem) -> Result<String, RuntimeError> {
    serde_json::to_string(&LogLineRef {
        schema: SCHEMA_VERSION,
        item,
    })
    .map_err(|error| RuntimeError::Store(error.to_string()))
}

/// The content hash debug builds use to verify that logged entries never
/// change. Release builds skip the serialization and store a constant —
/// membership by id is the whole release-mode contract.
fn verification_hash(item: &TranscriptItem) -> Result<u64, RuntimeError> {
    if cfg!(debug_assertions) {
        let canonical =
            serde_json::to_string(item).map_err(|error| RuntimeError::Store(error.to_string()))?;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        canonical.hash(&mut hasher);
        Ok(hasher.finish())
    } else {
        Ok(0)
    }
}
