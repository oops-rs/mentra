//! A file-backed [`RuntimeStore`](super::RuntimeStore) that links no database.
//!
//! [`FileRuntimeStore`] persists agents, transcripts, permission rules, and
//! run lifecycle events as plain files under one root the host names —
//! inspectable, greppable, `jq`-able:
//!
//! ```text
//! <root>/
//!   agents/<agent-id>/
//!     agent.json        the agent record and its store timestamps   (atomic replace)
//!     state.json        working memory minus entry contents         (atomic replace)
//!     transcript.jsonl  one transcript entry per line               (append-only)
//!     leaf              the active entry id                         (atomic replace)
//!   rules.json          durable permission rules                    (atomic replace)
//!   runs.jsonl          run lifecycle events                        (append-only)
//! ```
//!
//! Every file carries a `schema` field (currently 1) so a later reader can
//! tell what it is looking at; `transcript.jsonl` carries it per line.
//!
//! ## Durability model
//!
//! Appending transcript entries is the common write: each save appends the
//! entries the log does not yet hold, then replaces `state.json`, then
//! `leaf`. Atomic replaces go through a temp file in the same directory,
//! fsync, and rename; appends fsync at each commit point. A crash leaves at
//! most a truncated final `transcript.jsonl` line, which the reader skips
//! and the next append starts on a fresh line; a leftover temp file is
//! ignored by every reader. `state.json` is the authoritative snapshot of
//! which logged entries are live and in what order — the transcript tree in
//! `transcript.jsonl` is the full history (abandoned branches and entries
//! superseded by compaction or run rollback stay in the log), and `leaf` is
//! the maintained pointer a shell tool can read without parsing JSON.
//!
//! ## Concurrency
//!
//! Same stance as the SQLite store's documented one: one writer per agent,
//! which mentra's runtime already holds. The file store adds no cross-process
//! locking beyond its atomic writes — two processes (or two independently
//! constructed stores) writing the same agent directory concurrently are
//! outside the contract. Within one process, clones share state and writes
//! are serialized per agent.
//!
//! ## What this store deliberately does not persist
//!
//! Each subsystem trait keeps the behavior that is honest for a durable
//! store without a database (issue #28's cut: those subsystems stay on the
//! SQLite store, behind the `store-sqlite` feature):
//!
//! - **Tasks, teams, and background jobs** are kept in process memory,
//!   through the same mechanism [`VolatileRuntimeStore`] uses, and vanish
//!   with the process. Tasks and team state are working coordination inside
//!   one runtime; background processes die with the process that spawned
//!   them, so their notification queues do too.
//! - **Leases** are advisory OS file locks under `leases/` — real
//!   cross-process exclusion, released automatically when the holding
//!   process dies (see the `leases` module for why the TTL is ignored and
//!   lock files are never unlinked).
//! - **Audit events** are accepted and discarded, as the volatile store
//!   does: the trait has no reader, so refusing the write would only break
//!   hosts that emit events, without making anything more durable.
//! - **Long-term memory** is refused with an error naming the fix: durable
//!   memory search is what the SQLite FTS index is for, and a "long-term"
//!   memory that silently forgot on restart would be worse than saying no.
//!   The runtime degrades gracefully around the refusal: automatic recall
//!   and ingest carry on without records, and an applied compaction whose
//!   summary write is refused still stands (the failure is reported through
//!   the memory hook events); the memory tools report the error text.

mod agent;
mod delegated;
mod fs_util;
mod leases;
mod rules;
mod runs;
#[cfg(test)]
mod tests;
mod transcript_log;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use super::error::RuntimeError;
use super::store::default_store_dir;
use super::volatile_store::VolatileRuntimeStore;
use transcript_log::TranscriptLogIndex;

/// Newest file layout revision this build writes and understands.
const SCHEMA_VERSION: u32 = 1;

/// A [`RuntimeStore`](super::RuntimeStore) backed by plain files under one
/// root directory. See the module docs for the layout, the durability model,
/// and what is deliberately kept volatile or refused.
#[derive(Clone)]
pub struct FileRuntimeStore {
    root: PathBuf,
    /// In-process subsystems (tasks, teams, background jobs, leases) — the
    /// volatile store already implements exactly the behavior this store
    /// keeps for them.
    volatile: VolatileRuntimeStore,
    /// Per-agent index of which transcript entries the on-disk log already
    /// holds, so a save appends only what is new or changed.
    transcript_logs: Arc<Mutex<HashMap<String, Arc<Mutex<TranscriptLogIndex>>>>>,
    /// Serializes read-modify-write cycles on `rules.json`.
    rules_lock: Arc<Mutex<()>>,
    /// The OS file locks this store currently holds as leases; dropping an
    /// entry (or the whole store, or the process) releases the lock.
    held_leases: Arc<Mutex<HashMap<String, leases::HeldLease>>>,
    /// Serializes appends to `runs.jsonl`.
    runs_lock: Arc<Mutex<()>>,
}

impl FileRuntimeStore {
    /// Creates a file store rooted at `root`. Nothing is created until the
    /// store is first written to or prepared for recovery.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            volatile: VolatileRuntimeStore::new(),
            transcript_logs: Arc::new(Mutex::new(HashMap::new())),
            rules_lock: Arc::new(Mutex::new(())),
            held_leases: Arc::new(Mutex::new(HashMap::new())),
            runs_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Returns the default root used when no explicit one is provided: the
    /// same per-workspace directory the SQLite default database lives in.
    pub fn default_root() -> PathBuf {
        default_store_dir()
    }

    /// The root directory holding this store's files.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn agents_dir(&self) -> PathBuf {
        self.root.join("agents")
    }

    pub(crate) fn agent_dir(&self, agent_id: &str) -> PathBuf {
        self.agents_dir().join(fs_util::encode_component(agent_id))
    }

    pub(crate) fn rules_path(&self) -> PathBuf {
        self.root.join("rules.json")
    }

    pub(crate) fn runs_path(&self) -> PathBuf {
        self.root.join("runs.jsonl")
    }

    pub(crate) fn leases_dir(&self) -> PathBuf {
        self.root.join("leases")
    }

    /// The shared per-agent transcript-log index handle.
    fn transcript_log(&self, agent_id: &str) -> Arc<Mutex<TranscriptLogIndex>> {
        let mut logs = lock_unpoisoned(&self.transcript_logs);
        logs.entry(agent_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(TranscriptLogIndex::default())))
            .clone()
    }

    fn forget_transcript_log(&self, agent_id: &str) {
        lock_unpoisoned(&self.transcript_logs).remove(agent_id);
    }

    /// Drops every lease this store holds, playing the part of the previous
    /// holding process having died. The store-parameterized suites hand one
    /// store to two runtimes in sequence and use this the way their SQLite
    /// variant DELETEs lease rows.
    #[cfg(all(test, not(feature = "store-sqlite")))]
    pub(crate) fn release_all_leases(&self) {
        lock_unpoisoned(&self.held_leases).clear();
    }
}

impl Default for FileRuntimeStore {
    /// A file store under the default per-workspace root — the same default
    /// path policy the SQLite store's default database uses.
    fn default() -> Self {
        Self::new(Self::default_root())
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn store_error(context: &str, error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Store(format!("{context}: {error}"))
}

/// Parses a whole-file JSON payload after checking its `schema` field, so a
/// file written by a newer layout is refused by name rather than misread.
fn parse_versioned<T: serde::de::DeserializeOwned>(
    contents: &str,
    file: &str,
) -> Result<T, RuntimeError> {
    #[derive(serde::Deserialize)]
    struct SchemaOnly {
        schema: u32,
    }
    let schema: SchemaOnly = serde_json::from_str(contents)
        .map_err(|error| store_error(&format!("parse {file}"), error))?;
    if schema.schema > SCHEMA_VERSION {
        return Err(RuntimeError::Store(format!(
            "{file} schema {} is newer than this build understands ({SCHEMA_VERSION})",
            schema.schema
        )));
    }
    serde_json::from_str(contents).map_err(|error| store_error(&format!("parse {file}"), error))
}

fn to_pretty_json<T: serde::Serialize>(value: &T) -> Result<String, RuntimeError> {
    serde_json::to_string_pretty(value).map_err(|error| RuntimeError::Store(error.to_string()))
}
