//! The append-only transcript entry log (`transcript.jsonl`).
//!
//! Each line is one [`TranscriptItem`] with a `schema` field added, keyed by
//! its entry id. The log is history: entries are appended when first seen
//! (or when their content changed, which the runtime never does today but a
//! log must not silently miss) and never rewritten, so abandoned branches
//! and entries superseded by compaction or a run rollback remain greppable.
//! On replay the last occurrence of an id wins; a truncated final line —
//! the only damage a crashed append can leave — is skipped.

use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    path::Path,
};

use crate::{runtime::error::RuntimeError, transcript::TranscriptItem};

use super::{SCHEMA_VERSION, fs_util, store_error};

/// What the store remembers about one agent's on-disk log: which entry ids
/// it holds, hashed by their canonical serialization so a changed entry is
/// re-appended rather than assumed unchanged.
#[derive(Default)]
pub(super) struct TranscriptLogIndex {
    loaded: bool,
    entries: HashMap<String, u64>,
}

impl TranscriptLogIndex {
    /// Loads the index from the log file on first use.
    pub(super) fn ensure_loaded(&mut self, path: &Path) -> Result<(), RuntimeError> {
        if self.loaded {
            return Ok(());
        }
        self.entries = read_log(path)?
            .into_iter()
            .map(|(id, item)| Ok((id, hash_entry(&canonical_json(&item)?))))
            .collect::<Result<_, RuntimeError>>()?;
        self.loaded = true;
        Ok(())
    }

    /// Appends every entry the log does not already hold in this exact form,
    /// in the order given, fsyncing once. Duplicate ids within one call are
    /// appended once.
    pub(super) fn append_missing<'a>(
        &mut self,
        path: &Path,
        entries: impl Iterator<Item = &'a TranscriptItem>,
    ) -> Result<(), RuntimeError> {
        self.ensure_loaded(path)?;
        let mut lines = Vec::new();
        let mut pending: Vec<(String, u64)> = Vec::new();
        for item in entries {
            let id = item.id.as_str().to_string();
            let canonical = canonical_json(item)?;
            let hash = hash_entry(&canonical);
            let already_pending = pending.iter().any(|(pending_id, _)| pending_id == &id);
            if already_pending || self.entries.get(&id) == Some(&hash) {
                continue;
            }
            lines.push(log_line(&canonical)?);
            pending.push((id, hash));
        }
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
    let mut value: serde_json::Value =
        serde_json::from_str(line).map_err(|error| RuntimeError::Store(error.to_string()))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| RuntimeError::Store("transcript line is not an object".to_string()))?;
    let schema = object
        .remove("schema")
        .and_then(|schema| schema.as_u64())
        .ok_or_else(|| RuntimeError::Store("transcript line has no schema field".to_string()))?;
    if schema > u64::from(SCHEMA_VERSION) {
        return Err(RuntimeError::Store(format!(
            "transcript line schema {schema} is newer than this build understands ({SCHEMA_VERSION})"
        )));
    }
    serde_json::from_value(value).map_err(|error| RuntimeError::Store(error.to_string()))
}

fn log_line(canonical: &str) -> Result<String, RuntimeError> {
    let mut value: serde_json::Value =
        serde_json::from_str(canonical).map_err(|error| RuntimeError::Store(error.to_string()))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| RuntimeError::Store("transcript entry is not an object".to_string()))?;
    object.insert("schema".to_string(), SCHEMA_VERSION.into());
    serde_json::to_string(&value).map_err(|error| RuntimeError::Store(error.to_string()))
}

/// The comparison form of an entry: its serde JSON, which is deterministic
/// for the same content.
fn canonical_json(item: &TranscriptItem) -> Result<String, RuntimeError> {
    serde_json::to_string(item).map_err(|error| RuntimeError::Store(error.to_string()))
}

fn hash_entry(canonical: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut hasher);
    hasher.finish()
}
