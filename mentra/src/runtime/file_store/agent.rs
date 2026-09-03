//! [`AgentStore`] on files: `agent.json` is the record and the commit point
//! of agent creation, `state.json` the authoritative working-memory snapshot
//! (entry ids only), `transcript.jsonl` the entry contents, `leaf` the
//! active entry id for tools that read no JSON.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{
    Message,
    memory::journal::{AgentMemoryState, CompactionState, PendingTurnState, RunMemoryState},
    session::PermissionRuleScope,
    transcript::{AgentTranscript, EntryId, TranscriptItem},
};

use super::{
    super::store::{
        AgentStore, LoadedAgentState, PermissionRuleContext, PermissionRuleStore,
        PersistedAgentRecord, now_secs,
    },
    FileRuntimeStore, RuntimeError, SCHEMA_VERSION, fs_util, lock_unpoisoned, parse_versioned,
    store_error, to_pretty_json, transcript_log,
};

const AGENT_FILE: &str = "agent.json";
const STATE_FILE: &str = "state.json";
const TRANSCRIPT_FILE: &str = "transcript.jsonl";
const LEAF_FILE: &str = "leaf";

#[derive(Serialize, Deserialize)]
struct AgentFile {
    schema: u32,
    /// When this store first wrote the agent, in seconds since the epoch.
    created_at: u64,
    /// When this store last wrote the agent record.
    updated_at: u64,
    record: PersistedAgentRecord,
}

/// `state.json`: everything [`AgentMemoryState`] holds except transcript
/// entry contents, which live in the log and are named here by id. One
/// atomic file, so a crash between writes can never mix two snapshots.
#[derive(Serialize, Deserialize)]
struct StateFile {
    schema: u32,
    revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_turn: Option<PendingTurnState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resumable_user_message: Option<Message>,
    #[serde(default)]
    compaction: CompactionState,
    transcript: TranscriptShape,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run: Option<RunFile>,
}

/// A transcript by reference: the active path root to leaf, then the
/// archived entries, both in their in-memory order.
#[derive(Serialize, Deserialize)]
struct TranscriptShape {
    items: Vec<EntryId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    archive: Vec<EntryId>,
}

#[derive(Serialize, Deserialize)]
struct RunFile {
    run_id: String,
    assistant_committed: bool,
    baseline: TranscriptShape,
}

impl AgentStore for FileRuntimeStore {
    fn prepare_recovery(&self) -> Result<(), RuntimeError> {
        // With the SQLite store compiled out, an existing runtime.sqlite in
        // this root means previous sessions live in a database this build
        // cannot read. Starting an empty file store beside it would look
        // exactly like silent data loss, so name the situation and the two
        // ways out instead. With the feature on, the check would misfire:
        // both defaults share this root, and running the file store beside
        // a SQLite database is then a legitimate arrangement.
        #[cfg(not(feature = "store-sqlite"))]
        {
            let sqlite_db = self.root().join("runtime.sqlite");
            if sqlite_db.exists() {
                return Err(RuntimeError::Store(format!(
                    "'{}' holds an existing SQLite runtime store, but this build compiled \
                     mentra without the `store-sqlite` feature; enable that feature to keep \
                     reading it, or point the file store at a different root",
                    sqlite_db.display()
                )));
            }
        }

        // Mirrors what opening the SQLite database does at build time: the
        // store's home is created and writable, so a misconfigured root
        // fails the build rather than the first turn. The queues the SQLite
        // store reconciles here (team inbox, background notifications) are
        // in-process for this store and start empty, and a lease dies with
        // the process that held it.
        let agents_dir = self.agents_dir();
        std::fs::create_dir_all(&agents_dir)
            .map_err(|error| store_error(&format!("create '{}'", agents_dir.display()), error))
    }

    fn create_agent(
        &self,
        record: &PersistedAgentRecord,
        memory: &AgentMemoryState,
    ) -> Result<(), RuntimeError> {
        // Memory first, record last: `agent.json` is the commit point, so a
        // crash mid-create leaves a directory that no reader treats as an
        // agent rather than a record whose memory is missing.
        self.save_memory(&record.id, memory)?;
        self.save_record(record)
    }

    fn save_agent_record(&self, record: &PersistedAgentRecord) -> Result<(), RuntimeError> {
        self.save_record(record)
    }

    fn save_agent_memory(
        &self,
        agent_id: &str,
        memory: &AgentMemoryState,
    ) -> Result<(), RuntimeError> {
        self.save_memory(agent_id, memory)
    }

    fn load_agent(&self, agent_id: &str) -> Result<Option<LoadedAgentState>, RuntimeError> {
        let dir = self.agent_dir(agent_id);
        let Some(contents) = fs_util::read_optional(&dir.join(AGENT_FILE))? else {
            return Ok(None);
        };
        let agent_file: AgentFile = parse_versioned(&contents, AGENT_FILE)?;
        Ok(Some(finish_load(&dir, agent_file)?))
    }

    fn delete_agent(&self, agent_id: &str) -> Result<(), RuntimeError> {
        self.forget_transcript_log(agent_id);
        let dir = self.agent_dir(agent_id);

        // agent.json is the commit point of creation, so it goes first on
        // deletion too: a delete that crashes midway then leaves the same
        // record-less directory readers already ignore, never a listed
        // agent with half its files gone.
        let agent_path = dir.join(AGENT_FILE);
        match std::fs::remove_file(&agent_path) {
            Ok(()) => fs_util::fsync_dir(&dir)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(store_error(
                    &format!("remove '{}'", agent_path.display()),
                    error,
                ));
            }
        }

        match std::fs::remove_dir_all(&dir) {
            Ok(()) => fs_util::fsync_dir(&self.agents_dir()),
            // The caller's goal is that the agent be gone; it already is.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(store_error(&format!("remove '{}'", dir.display()), error)),
        }?;

        // The record is the deletion commit point. Clear its owned rules only
        // after that commit so a rule-store failure cannot strip permissions
        // from an agent that remains resumable. A retry against an absent
        // record reaches this step again and finishes the orphan cleanup.
        self.clear_scope(
            &PermissionRuleContext {
                session_id: agent_id.to_owned(),
                project_id: None,
            },
            PermissionRuleScope::Session,
        )?;
        Ok(())
    }

    fn list_agents(&self) -> Result<Vec<LoadedAgentState>, RuntimeError> {
        let agents_dir = self.agents_dir();
        let entries = match std::fs::read_dir(&agents_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(store_error(
                    &format!("list '{}'", agents_dir.display()),
                    error,
                ));
            }
        };

        let mut agents = Vec::new();
        for entry in entries {
            let entry = entry
                .map_err(|error| store_error(&format!("list '{}'", agents_dir.display()), error))?;
            let dir = entry.path();
            // Only a directory holding an agent.json is an agent; anything
            // else — a stray file, a crashed create, a leftover temp — is not.
            let agent_path = dir.join(AGENT_FILE);
            if !dir.is_dir() || !agent_path.is_file() {
                continue;
            }
            let Some(contents) = fs_util::read_optional(&agent_path)? else {
                continue;
            };
            let agent_file: AgentFile = parse_versioned(&contents, AGENT_FILE)?;
            agents.push(finish_load(&dir, agent_file)?);
        }

        agents.sort_by(|a, b| (a.created_at, &a.record.id).cmp(&(b.created_at, &b.record.id)));
        Ok(agents)
    }

    fn list_agents_by_runtime(
        &self,
        runtime_identifier: &str,
    ) -> Result<Vec<LoadedAgentState>, RuntimeError> {
        Ok(self
            .list_agents()?
            .into_iter()
            .filter(|loaded| loaded.record.runtime_identifier == runtime_identifier)
            .collect())
    }
}

impl FileRuntimeStore {
    fn save_record(&self, record: &PersistedAgentRecord) -> Result<(), RuntimeError> {
        let path = self.agent_dir(&record.id).join(AGENT_FILE);
        let now = now_secs() as u64;
        // Mirrors the SQLite upsert: the first write settles created_at, and
        // every later one moves only updated_at.
        let created_at = match fs_util::read_optional(&path)? {
            Some(contents) => parse_versioned::<AgentFile>(&contents, AGENT_FILE)?.created_at,
            None => now,
        };
        let file = AgentFile {
            schema: SCHEMA_VERSION,
            created_at,
            updated_at: now,
            record: record.clone(),
        };
        fs_util::atomic_replace(&path, to_pretty_json(&file)?.as_bytes())
    }

    fn save_memory(&self, agent_id: &str, memory: &AgentMemoryState) -> Result<(), RuntimeError> {
        let dir = self.agent_dir(agent_id);
        let log = self.transcript_log(agent_id);
        let mut index = lock_unpoisoned(&log);

        // 1. The entry log gains whatever this snapshot references that it
        //    does not already hold.
        index.append_missing(&dir.join(TRANSCRIPT_FILE), entries_of(memory))?;

        // 2. The snapshot itself, atomically: after this write the new state
        //    is what loads; before it, the old one still does.
        let state_file = decompose_memory(memory);
        fs_util::atomic_replace(
            &dir.join(STATE_FILE),
            to_pretty_json(&state_file)?.as_bytes(),
        )?;

        // 3. The convenience pointer, kept last so it never names an entry
        //    the log has not committed.
        let leaf_path = dir.join(LEAF_FILE);
        match memory.transcript.leaf() {
            Some(leaf) => {
                fs_util::atomic_replace(&leaf_path, format!("{leaf}\n").as_bytes())?;
            }
            None => match std::fs::remove_file(&leaf_path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(store_error(
                        &format!("remove '{}'", leaf_path.display()),
                        error,
                    ));
                }
            },
        }
        Ok(())
    }
}

/// Completes a load whose record file is already parsed: reads the memory
/// snapshot and the entry log out of `dir` and assembles the loaded state.
/// Shared by [`AgentStore::load_agent`] and the listing walk, so a listed
/// agent is parsed once rather than twice.
fn finish_load(
    dir: &std::path::Path,
    agent_file: AgentFile,
) -> Result<LoadedAgentState, RuntimeError> {
    let Some(state_contents) = fs_util::read_optional(&dir.join(STATE_FILE))? else {
        return Err(RuntimeError::Store(format!(
            "Agent '{}' is missing persisted memory",
            agent_file.record.id
        )));
    };
    let state_file: StateFile = parse_versioned(&state_contents, STATE_FILE)?;
    let entries = transcript_log::read_log(&dir.join(TRANSCRIPT_FILE))?;
    let memory = compose_memory(state_file, &entries)?;

    Ok(LoadedAgentState {
        record: agent_file.record,
        memory,
        created_at: Some(agent_file.created_at),
        updated_at: Some(agent_file.updated_at),
    })
}

/// Every transcript entry a memory snapshot references: the live tree and,
/// during a run, the baseline it would roll back to.
fn entries_of(memory: &AgentMemoryState) -> impl Iterator<Item = &TranscriptItem> {
    let baseline = memory.run.as_ref().map(|run| &run.baseline_transcript);
    memory
        .transcript
        .items()
        .iter()
        .chain(memory.transcript.archived())
        .chain(
            baseline
                .into_iter()
                .flat_map(|transcript| transcript.items().iter().chain(transcript.archived())),
        )
}

fn decompose_memory(memory: &AgentMemoryState) -> StateFile {
    StateFile {
        schema: SCHEMA_VERSION,
        revision: memory.revision,
        pending_turn: memory.pending_turn.clone(),
        resumable_user_message: memory.resumable_user_message.clone(),
        compaction: memory.compaction.clone(),
        transcript: shape_of(&memory.transcript),
        run: memory.run.as_ref().map(|run| RunFile {
            run_id: run.run_id.clone(),
            assistant_committed: run.assistant_committed,
            baseline: shape_of(&run.baseline_transcript),
        }),
    }
}

fn compose_memory(
    state: StateFile,
    entries: &HashMap<String, TranscriptItem>,
) -> Result<AgentMemoryState, RuntimeError> {
    Ok(AgentMemoryState {
        transcript: transcript_from(&state.transcript, entries)?,
        pending_turn: state.pending_turn,
        resumable_user_message: state.resumable_user_message,
        compaction: state.compaction,
        revision: state.revision,
        run: state
            .run
            .map(|run| {
                Ok::<_, RuntimeError>(RunMemoryState {
                    run_id: run.run_id,
                    baseline_transcript: transcript_from(&run.baseline, entries)?,
                    assistant_committed: run.assistant_committed,
                })
            })
            .transpose()?,
    })
}

fn shape_of(transcript: &AgentTranscript) -> TranscriptShape {
    TranscriptShape {
        items: transcript
            .items()
            .iter()
            .map(|item| item.id.clone())
            .collect(),
        archive: transcript
            .archived()
            .iter()
            .map(|item| item.id.clone())
            .collect(),
    }
}

fn transcript_from(
    shape: &TranscriptShape,
    entries: &HashMap<String, TranscriptItem>,
) -> Result<AgentTranscript, RuntimeError> {
    let resolve = |id: &EntryId| {
        entries.get(id.as_str()).cloned().ok_or_else(|| {
            RuntimeError::Store(format!(
                "transcript entry '{id}' is named by state.json but missing from transcript.jsonl"
            ))
        })
    };
    Ok(AgentTranscript::from_parts(
        shape.items.iter().map(resolve).collect::<Result<_, _>>()?,
        shape
            .archive
            .iter()
            .map(resolve)
            .collect::<Result<_, _>>()?,
    ))
}
