//! The subsystems the file store deliberately does not put on disk (see the
//! module docs for the reasoning per trait): tasks, teams, background jobs,
//! and leases run through the embedded volatile mechanism; audit events are
//! accepted and discarded; long-term memory is refused by name.

use std::{path::Path, time::Duration};

use crate::{
    background::{BackgroundNotification, BackgroundStore, BackgroundTaskSummary},
    memory::{MemoryCursor, MemoryRecord, MemorySearchRequest, MemoryStore},
    team::{TeamMemberSummary, TeamMessage, TeamProtocolRequestSummary, TeamStore},
};

use super::{
    super::store::{AuditStore, LeaseStore, TaskStateSnapshot, TaskStore},
    FileRuntimeStore, RuntimeError,
};
use crate::runtime::TaskItem;

impl TaskStore for FileRuntimeStore {
    fn load_tasks(&self, namespace: &Path) -> Result<Vec<TaskItem>, RuntimeError> {
        self.volatile.load_tasks(namespace)
    }

    fn capture_tasks(&self, namespace: &Path) -> Result<TaskStateSnapshot, RuntimeError> {
        self.volatile.capture_tasks(namespace)
    }

    fn restore_tasks(
        &self,
        namespace: &Path,
        snapshot: &TaskStateSnapshot,
    ) -> Result<(), RuntimeError> {
        self.volatile.restore_tasks(namespace, snapshot)
    }

    fn replace_tasks(&self, namespace: &Path, tasks: &[TaskItem]) -> Result<(), RuntimeError> {
        self.volatile.replace_tasks(namespace, tasks)
    }

    fn mutate(
        &self,
        namespace: &Path,
        mutation: &mut dyn FnMut(&mut Vec<TaskItem>) -> Result<(), RuntimeError>,
    ) -> Result<(), RuntimeError> {
        self.volatile.mutate(namespace, mutation)
    }
}

impl TeamStore for FileRuntimeStore {
    fn unread_team_count(&self, team_dir: &Path, agent_name: &str) -> Result<usize, RuntimeError> {
        self.volatile.unread_team_count(team_dir, agent_name)
    }

    fn load_team_members(&self, team_dir: &Path) -> Result<Vec<TeamMemberSummary>, RuntimeError> {
        self.volatile.load_team_members(team_dir)
    }

    fn upsert_team_member(
        &self,
        team_dir: &Path,
        summary: &TeamMemberSummary,
    ) -> Result<(), RuntimeError> {
        self.volatile.upsert_team_member(team_dir, summary)
    }

    fn read_team_inbox(
        &self,
        team_dir: &Path,
        agent_name: &str,
    ) -> Result<Vec<TeamMessage>, RuntimeError> {
        self.volatile.read_team_inbox(team_dir, agent_name)
    }

    fn ack_team_inbox(&self, team_dir: &Path, agent_name: &str) -> Result<(), RuntimeError> {
        self.volatile.ack_team_inbox(team_dir, agent_name)
    }

    fn requeue_team_inbox(&self, team_dir: &Path, agent_name: &str) -> Result<(), RuntimeError> {
        self.volatile.requeue_team_inbox(team_dir, agent_name)
    }

    fn append_team_message(
        &self,
        team_dir: &Path,
        recipient: &str,
        message: &TeamMessage,
    ) -> Result<(), RuntimeError> {
        self.volatile
            .append_team_message(team_dir, recipient, message)
    }

    fn load_team_requests(
        &self,
        team_dir: &Path,
    ) -> Result<Vec<TeamProtocolRequestSummary>, RuntimeError> {
        self.volatile.load_team_requests(team_dir)
    }

    fn upsert_team_request(
        &self,
        team_dir: &Path,
        request: &TeamProtocolRequestSummary,
    ) -> Result<(), RuntimeError> {
        self.volatile.upsert_team_request(team_dir, request)
    }

    fn list_team_agent_names(&self, team_dir: &Path) -> Result<Vec<String>, RuntimeError> {
        self.volatile.list_team_agent_names(team_dir)
    }
}

impl BackgroundStore for FileRuntimeStore {
    fn load_background_tasks(
        &self,
        agent_id: &str,
    ) -> Result<Vec<BackgroundTaskSummary>, RuntimeError> {
        self.volatile.load_background_tasks(agent_id)
    }

    fn upsert_background_task(
        &self,
        agent_id: &str,
        task: &BackgroundTaskSummary,
        notification_state: i64,
    ) -> Result<(), RuntimeError> {
        self.volatile
            .upsert_background_task(agent_id, task, notification_state)
    }

    fn drain_background_notifications(
        &self,
        agent_id: &str,
    ) -> Result<Vec<BackgroundNotification>, RuntimeError> {
        self.volatile.drain_background_notifications(agent_id)
    }

    fn has_pending_background_notifications(&self, agent_id: &str) -> Result<bool, RuntimeError> {
        self.volatile.has_pending_background_notifications(agent_id)
    }

    fn has_deliverable_background_notifications(
        &self,
        agent_id: &str,
    ) -> Result<bool, RuntimeError> {
        self.volatile
            .has_deliverable_background_notifications(agent_id)
    }

    fn ack_background_notifications(&self, agent_id: &str) -> Result<(), RuntimeError> {
        self.volatile.ack_background_notifications(agent_id)
    }

    fn requeue_background_notifications(&self, agent_id: &str) -> Result<(), RuntimeError> {
        self.volatile.requeue_background_notifications(agent_id)
    }
}

impl LeaseStore for FileRuntimeStore {
    fn acquire_lease(&self, key: &str, owner: &str, ttl: Duration) -> Result<bool, RuntimeError> {
        self.volatile.acquire_lease(key, owner, ttl)
    }

    fn release_lease(&self, key: &str, owner: &str) -> Result<(), RuntimeError> {
        self.volatile.release_lease(key, owner)
    }
}

impl AuditStore for FileRuntimeStore {
    fn record_audit_event(
        &self,
        _scope: &str,
        _event_type: &str,
        _payload: serde_json::Value,
    ) -> Result<(), RuntimeError> {
        // Accepted and discarded, as the volatile store does: `AuditStore`
        // has no reader, and refusing the write would break every host that
        // emits events without making anything more durable. Hosts that need
        // an audit trail keep the SQLite store (`store-sqlite`).
        Ok(())
    }
}

impl MemoryStore for FileRuntimeStore {
    fn upsert_records(&self, _records: &[MemoryRecord]) -> Result<(), RuntimeError> {
        Err(memory_unavailable())
    }

    fn search_records_with_options(
        &self,
        _request: &MemorySearchRequest,
    ) -> Result<Vec<MemoryRecord>, RuntimeError> {
        Err(memory_unavailable())
    }

    fn delete_records(&self, _record_ids: &[String]) -> Result<(), RuntimeError> {
        Err(memory_unavailable())
    }

    fn tombstone_records(
        &self,
        _agent_id: &str,
        _record_ids: &[String],
    ) -> Result<usize, RuntimeError> {
        Err(memory_unavailable())
    }

    fn load_agent_memory_cursor(
        &self,
        _agent_id: &str,
    ) -> Result<Option<MemoryCursor>, RuntimeError> {
        Err(memory_unavailable())
    }

    fn save_agent_memory_cursor(
        &self,
        _agent_id: &str,
        _cursor: &MemoryCursor,
    ) -> Result<(), RuntimeError> {
        Err(memory_unavailable())
    }
}

/// Long-term memory is refused rather than kept volatile: a durable store
/// whose "long-term" memory silently vanished on restart would be lying,
/// and the search quality the memory engine is built around is the SQLite
/// FTS index. The runtime degrades around this error on its automatic
/// paths — recall and ingest proceed without records, and an applied
/// compaction survives its refused summary write — while the memory tools
/// surface the text.
fn memory_unavailable() -> RuntimeError {
    RuntimeError::Store(
        "FileRuntimeStore does not persist long-term memory; enable mentra's `store-sqlite` \
         feature and use SqliteRuntimeStore or HybridRuntimeStore for durable memory"
            .to_string(),
    )
}
