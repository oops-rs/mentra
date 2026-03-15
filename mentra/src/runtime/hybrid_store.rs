use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use crate::{
    background::{BackgroundNotification, BackgroundStore, BackgroundTaskSummary},
    memory::{
        MemoryCursor, MemoryRecord, MemorySearchRequest, MemoryStore, SqliteHybridMemoryStore,
    },
    runtime::{
        AgentStore, AuditStore, LeaseStore, LoadedAgentState, PersistedAgentRecord, RunStore,
        SqliteRuntimeStore, TaskStateSnapshot, TaskStore,
    },
    team::{TeamMemberSummary, TeamMessage, TeamProtocolRequestSummary, TeamStore},
};

use super::{RuntimeError, TaskItem};

#[derive(Clone)]
/// Runtime store that keeps the existing SQLite runtime data path and swaps in a richer memory store.
pub struct HybridRuntimeStore {
    inner: SqliteRuntimeStore,
    memory: SqliteHybridMemoryStore,
}

impl Default for HybridRuntimeStore {
    fn default() -> Self {
        Self::new(SqliteRuntimeStore::default_path())
    }
}

impl HybridRuntimeStore {
    pub fn new(runtime_path: impl Into<PathBuf>) -> Self {
        let runtime_path = runtime_path.into();
        let memory_path = derive_memory_path(runtime_path.as_path());
        Self {
            inner: SqliteRuntimeStore::new(runtime_path),
            memory: SqliteHybridMemoryStore::new(memory_path),
        }
    }

    pub fn with_memory_path(
        runtime_path: impl Into<PathBuf>,
        memory_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            inner: SqliteRuntimeStore::new(runtime_path),
            memory: SqliteHybridMemoryStore::new(memory_path),
        }
    }

    pub fn for_runtime_identifier(runtime_identifier: &str) -> Self {
        Self::new(SqliteRuntimeStore::path_for_runtime_identifier(
            runtime_identifier,
        ))
    }

    pub fn runtime_path(&self) -> &Path {
        self.inner.path()
    }

    pub fn memory_path(&self) -> &Path {
        self.memory.path()
    }
}

impl TeamStore for HybridRuntimeStore {
    fn unread_team_count(&self, team_dir: &Path, agent_name: &str) -> Result<usize, RuntimeError> {
        self.inner.unread_team_count(team_dir, agent_name)
    }

    fn load_team_members(&self, team_dir: &Path) -> Result<Vec<TeamMemberSummary>, RuntimeError> {
        self.inner.load_team_members(team_dir)
    }

    fn upsert_team_member(
        &self,
        team_dir: &Path,
        summary: &TeamMemberSummary,
    ) -> Result<(), RuntimeError> {
        self.inner.upsert_team_member(team_dir, summary)
    }

    fn read_team_inbox(
        &self,
        team_dir: &Path,
        agent_name: &str,
    ) -> Result<Vec<TeamMessage>, RuntimeError> {
        self.inner.read_team_inbox(team_dir, agent_name)
    }

    fn ack_team_inbox(&self, team_dir: &Path, agent_name: &str) -> Result<(), RuntimeError> {
        self.inner.ack_team_inbox(team_dir, agent_name)
    }

    fn requeue_team_inbox(&self, team_dir: &Path, agent_name: &str) -> Result<(), RuntimeError> {
        self.inner.requeue_team_inbox(team_dir, agent_name)
    }

    fn append_team_message(
        &self,
        team_dir: &Path,
        recipient: &str,
        message: &TeamMessage,
    ) -> Result<(), RuntimeError> {
        self.inner.append_team_message(team_dir, recipient, message)
    }

    fn load_team_requests(
        &self,
        team_dir: &Path,
    ) -> Result<Vec<TeamProtocolRequestSummary>, RuntimeError> {
        self.inner.load_team_requests(team_dir)
    }

    fn upsert_team_request(
        &self,
        team_dir: &Path,
        request: &TeamProtocolRequestSummary,
    ) -> Result<(), RuntimeError> {
        self.inner.upsert_team_request(team_dir, request)
    }

    fn list_team_agent_names(&self, team_dir: &Path) -> Result<Vec<String>, RuntimeError> {
        self.inner.list_team_agent_names(team_dir)
    }
}

impl BackgroundStore for HybridRuntimeStore {
    fn load_background_tasks(
        &self,
        agent_id: &str,
    ) -> Result<Vec<BackgroundTaskSummary>, RuntimeError> {
        self.inner.load_background_tasks(agent_id)
    }

    fn upsert_background_task(
        &self,
        agent_id: &str,
        task: &BackgroundTaskSummary,
        notification_state: i64,
    ) -> Result<(), RuntimeError> {
        self.inner
            .upsert_background_task(agent_id, task, notification_state)
    }

    fn drain_background_notifications(
        &self,
        agent_id: &str,
    ) -> Result<Vec<BackgroundNotification>, RuntimeError> {
        self.inner.drain_background_notifications(agent_id)
    }

    fn has_pending_background_notifications(&self, agent_id: &str) -> Result<bool, RuntimeError> {
        self.inner.has_pending_background_notifications(agent_id)
    }

    fn ack_background_notifications(&self, agent_id: &str) -> Result<(), RuntimeError> {
        self.inner.ack_background_notifications(agent_id)
    }

    fn requeue_background_notifications(&self, agent_id: &str) -> Result<(), RuntimeError> {
        self.inner.requeue_background_notifications(agent_id)
    }
}

impl MemoryStore for HybridRuntimeStore {
    fn upsert_records(&self, records: &[MemoryRecord]) -> Result<(), RuntimeError> {
        self.memory.upsert_records(records)
    }

    fn search_records_with_options(
        &self,
        request: &MemorySearchRequest,
    ) -> Result<Vec<MemoryRecord>, RuntimeError> {
        self.memory.search_records_with_options(request)
    }

    fn search_records(
        &self,
        agent_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, RuntimeError> {
        self.memory.search_records(agent_id, query, limit)
    }

    fn delete_records(&self, record_ids: &[String]) -> Result<(), RuntimeError> {
        self.memory.delete_records(record_ids)
    }

    fn tombstone_records(
        &self,
        agent_id: &str,
        record_ids: &[String],
    ) -> Result<usize, RuntimeError> {
        self.memory.tombstone_records(agent_id, record_ids)
    }

    fn load_agent_memory_cursor(
        &self,
        agent_id: &str,
    ) -> Result<Option<MemoryCursor>, RuntimeError> {
        self.memory.load_agent_memory_cursor(agent_id)
    }

    fn save_agent_memory_cursor(
        &self,
        agent_id: &str,
        cursor: &MemoryCursor,
    ) -> Result<(), RuntimeError> {
        self.memory.save_agent_memory_cursor(agent_id, cursor)
    }
}

impl AgentStore for HybridRuntimeStore {
    fn prepare_recovery(&self) -> Result<(), RuntimeError> {
        self.inner.prepare_recovery()
    }

    fn create_agent(
        &self,
        record: &PersistedAgentRecord,
        memory: &crate::memory::journal::AgentMemoryState,
    ) -> Result<(), RuntimeError> {
        self.inner.create_agent(record, memory)
    }

    fn save_agent_record(&self, record: &PersistedAgentRecord) -> Result<(), RuntimeError> {
        self.inner.save_agent_record(record)
    }

    fn save_agent_memory(
        &self,
        agent_id: &str,
        memory: &crate::memory::journal::AgentMemoryState,
    ) -> Result<(), RuntimeError> {
        self.inner.save_agent_memory(agent_id, memory)
    }

    fn load_agent(&self, agent_id: &str) -> Result<Option<LoadedAgentState>, RuntimeError> {
        self.inner.load_agent(agent_id)
    }

    fn list_agents(&self) -> Result<Vec<LoadedAgentState>, RuntimeError> {
        self.inner.list_agents()
    }

    fn list_agents_by_runtime(
        &self,
        runtime_identifier: &str,
    ) -> Result<Vec<LoadedAgentState>, RuntimeError> {
        self.inner.list_agents_by_runtime(runtime_identifier)
    }
}

impl RunStore for HybridRuntimeStore {
    fn start_run(&self, agent_id: &str) -> Result<String, RuntimeError> {
        self.inner.start_run(agent_id)
    }

    fn update_run_state(
        &self,
        run_id: &str,
        state: &str,
        error: Option<&str>,
    ) -> Result<(), RuntimeError> {
        self.inner.update_run_state(run_id, state, error)
    }

    fn finish_run(&self, run_id: &str) -> Result<(), RuntimeError> {
        self.inner.finish_run(run_id)
    }

    fn fail_run(&self, run_id: &str, error: &str) -> Result<(), RuntimeError> {
        self.inner.fail_run(run_id, error)
    }
}

impl TaskStore for HybridRuntimeStore {
    fn load_tasks(&self, namespace: &Path) -> Result<Vec<TaskItem>, RuntimeError> {
        self.inner.load_tasks(namespace)
    }

    fn capture_tasks(&self, namespace: &Path) -> Result<TaskStateSnapshot, RuntimeError> {
        self.inner.capture_tasks(namespace)
    }

    fn restore_tasks(
        &self,
        namespace: &Path,
        snapshot: &TaskStateSnapshot,
    ) -> Result<(), RuntimeError> {
        self.inner.restore_tasks(namespace, snapshot)
    }

    fn replace_tasks(&self, namespace: &Path, tasks: &[TaskItem]) -> Result<(), RuntimeError> {
        self.inner.replace_tasks(namespace, tasks)
    }
}

impl AuditStore for HybridRuntimeStore {
    fn record_audit_event(
        &self,
        scope: &str,
        event_type: &str,
        payload: serde_json::Value,
    ) -> Result<(), RuntimeError> {
        self.inner.record_audit_event(scope, event_type, payload)
    }
}

impl LeaseStore for HybridRuntimeStore {
    fn acquire_lease(&self, key: &str, owner: &str, ttl: Duration) -> Result<bool, RuntimeError> {
        self.inner.acquire_lease(key, owner, ttl)
    }

    fn release_lease(&self, key: &str, owner: &str) -> Result<(), RuntimeError> {
        self.inner.release_lease(key, owner)
    }
}

fn derive_memory_path(runtime_path: &Path) -> PathBuf {
    let stem = runtime_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("runtime");
    let file_name = format!("{stem}-memory.sqlite");
    runtime_path.with_file_name(file_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::{AgentConfig, AgentStatus},
        runtime::AgentStore,
    };

    #[test]
    fn wrapper_store_delegates_non_memory_runtime_operations() {
        let base = std::env::temp_dir().join(format!(
            "mentra-hybrid-runtime-{}.sqlite",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let store = HybridRuntimeStore::new(base);
        let record = PersistedAgentRecord {
            id: "agent-1".to_string(),
            runtime_identifier: "default".to_string(),
            name: "agent".to_string(),
            model: "model".to_string(),
            provider_id: "anthropic".into(),
            config: AgentConfig::default(),
            hidden_tools: Default::default(),
            max_rounds: None,
            teammate_identity: None,
            rounds_since_task: 0,
            idle_requested: false,
            status: AgentStatus::Idle,
            subagents: Vec::new(),
        };
        store
            .create_agent(
                &record,
                &crate::memory::journal::AgentMemoryState::default(),
            )
            .expect("create agent");

        let loaded = store.load_agent("agent-1").expect("load agent");
        assert!(loaded.is_some());
        assert_ne!(store.runtime_path(), store.memory_path());
    }
}
