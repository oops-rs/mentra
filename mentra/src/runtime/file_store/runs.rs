//! [`RunStore`] as an append-only `runs.jsonl` event log.
//!
//! The SQLite store keeps one mutable row per run; nothing in mentra ever
//! reads a run back, so the honest file shape is the event log itself: a
//! start line when a run begins and one line per state transition, each
//! fsynced. A transition for a run id the log has never seen is still
//! recorded — after a restart, the runtime reports the interruption of a
//! run the previous process started, and the log should show that.

use serde::{Deserialize, Serialize};

use super::{
    super::store::{RunStore, next_id, now_secs},
    FileRuntimeStore, RuntimeError, SCHEMA_VERSION, fs_util, lock_unpoisoned,
};

#[derive(Serialize, Deserialize)]
struct RunEvent {
    schema: u32,
    /// Seconds since the epoch.
    at: i64,
    run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_id: Option<String>,
    state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl RunStore for FileRuntimeStore {
    fn start_run(&self, agent_id: &str) -> Result<String, RuntimeError> {
        let run_id = next_id("run");
        self.append_run_event(RunEvent {
            schema: SCHEMA_VERSION,
            at: now_secs(),
            run_id: run_id.clone(),
            agent_id: Some(agent_id.to_string()),
            state: "running".to_string(),
            error: None,
        })?;
        Ok(run_id)
    }

    fn update_run_state(
        &self,
        run_id: &str,
        state: &str,
        error: Option<&str>,
    ) -> Result<(), RuntimeError> {
        self.append_run_event(RunEvent {
            schema: SCHEMA_VERSION,
            at: now_secs(),
            run_id: run_id.to_string(),
            agent_id: None,
            state: state.to_string(),
            error: error.map(str::to_string),
        })
    }

    fn finish_run(&self, run_id: &str) -> Result<(), RuntimeError> {
        self.update_run_state(run_id, "finished", None)
    }

    fn fail_run(&self, run_id: &str, error: &str) -> Result<(), RuntimeError> {
        self.update_run_state(run_id, "failed", Some(error))
    }
}

impl FileRuntimeStore {
    fn append_run_event(&self, event: RunEvent) -> Result<(), RuntimeError> {
        let line = serde_json::to_string(&event)
            .map_err(|error| RuntimeError::Store(error.to_string()))?;
        let _guard = lock_unpoisoned(&self.runs_lock);
        fs_util::append_lines(&self.runs_path(), &[line])
    }
}
