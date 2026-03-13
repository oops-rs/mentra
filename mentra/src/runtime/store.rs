use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    agent::{AgentConfig, AgentMemoryState, AgentStatus, SpawnedAgentSummary, TeammateIdentity},
    background::{BackgroundNotification, BackgroundStore, BackgroundTaskStatus, BackgroundTaskSummary},
    provider::ProviderId,
    runtime::{
        TaskItem,
    },
    team::{TeamMemberSummary, TeamMessage, TeamProtocolRequestSummary, TeamStore},
};

use super::error::RuntimeError;

static NEXT_STORE_ID: AtomicU64 = AtomicU64::new(1);
#[cfg(test)]
static NEXT_TEST_STORE_ID: AtomicU64 = AtomicU64::new(1);

const DELIVERY_PENDING: i64 = 0;
const DELIVERY_INFLIGHT: i64 = 1;
const DELIVERY_ACKED: i64 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedAgentRecord {
    pub(crate) id: String,
    pub(crate) runtime_identifier: String,
    pub(crate) name: String,
    pub(crate) model: String,
    pub(crate) provider_id: ProviderId,
    pub(crate) config: AgentConfig,
    pub(crate) hidden_tools: HashSet<String>,
    pub(crate) max_rounds: Option<usize>,
    pub(crate) teammate_identity: Option<TeammateIdentity>,
    pub(crate) rounds_since_task: usize,
    pub(crate) idle_requested: bool,
    pub(crate) status: AgentStatus,
    pub(crate) subagents: Vec<SpawnedAgentSummary>,
}

#[derive(Debug, Clone)]
pub struct LoadedAgentState {
    pub(crate) record: PersistedAgentRecord,
    pub(crate) memory: AgentMemoryState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskStateSnapshot {
    pub(crate) tasks: Vec<TaskItem>,
}

/// Persistence backend used for agents, task state, audit events, and extracted domain stores.
pub trait RuntimeStore: TeamStore + BackgroundStore + Send + Sync {
    fn prepare_recovery(&self) -> Result<(), RuntimeError>;
    fn create_agent(
        &self,
        record: &PersistedAgentRecord,
        memory: &AgentMemoryState,
    ) -> Result<(), RuntimeError>;
    fn save_agent_record(&self, record: &PersistedAgentRecord) -> Result<(), RuntimeError>;
    fn save_agent_memory(
        &self,
        agent_id: &str,
        memory: &AgentMemoryState,
    ) -> Result<(), RuntimeError>;
    fn load_agent(&self, agent_id: &str) -> Result<Option<LoadedAgentState>, RuntimeError>;
    fn list_agents(&self) -> Result<Vec<LoadedAgentState>, RuntimeError>;
    fn list_agents_by_runtime(
        &self,
        runtime_identifier: &str,
    ) -> Result<Vec<LoadedAgentState>, RuntimeError>;
    fn start_run(&self, agent_id: &str) -> Result<String, RuntimeError>;
    fn update_run_state(
        &self,
        run_id: &str,
        state: &str,
        error: Option<&str>,
    ) -> Result<(), RuntimeError>;
    fn finish_run(&self, run_id: &str) -> Result<(), RuntimeError>;
    fn fail_run(&self, run_id: &str, error: &str) -> Result<(), RuntimeError>;
    fn load_tasks(&self, namespace: &Path) -> Result<Vec<TaskItem>, RuntimeError>;
    fn capture_tasks(&self, namespace: &Path) -> Result<TaskStateSnapshot, RuntimeError>;
    fn restore_tasks(
        &self,
        namespace: &Path,
        snapshot: &TaskStateSnapshot,
    ) -> Result<(), RuntimeError>;
    fn replace_tasks(&self, namespace: &Path, tasks: &[TaskItem]) -> Result<(), RuntimeError>;
    fn record_audit_event(
        &self,
        scope: &str,
        event_type: &str,
        payload: serde_json::Value,
    ) -> Result<(), RuntimeError>;
    fn acquire_lease(&self, key: &str, owner: &str, ttl: Duration) -> Result<bool, RuntimeError>;
    fn release_lease(&self, key: &str, owner: &str) -> Result<(), RuntimeError>;
}

impl TeamStore for SqliteRuntimeStore {
    fn unread_team_count(&self, team_dir: &Path, agent_name: &str) -> Result<usize, RuntimeError> {
        let conn = self.open()?;
        let count = conn
            .query_row(
                "SELECT COUNT(*) FROM team_inbox WHERE team_dir = ?1 AND recipient = ?2 AND delivery_state = ?3",
                params![Self::team_key(team_dir), agent_name, DELIVERY_PENDING],
                |row| row.get::<_, i64>(0),
            )
            .map_err(sqlite_error)?;
        Ok(count as usize)
    }

    fn load_team_members(&self, team_dir: &Path) -> Result<Vec<TeamMemberSummary>, RuntimeError> {
        let conn = self.open()?;
        let mut stmt = conn
            .prepare("SELECT summary_json FROM team_members WHERE team_dir = ?1 ORDER BY name")
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(params![Self::team_key(team_dir)], |row| {
                row.get::<_, String>(0)
            })
            .map_err(sqlite_error)?;
        let mut members = Vec::new();
        for row in rows {
            members.push(from_json(&row.map_err(sqlite_error)?)?);
        }
        Ok(members)
    }

    fn upsert_team_member(
        &self,
        team_dir: &Path,
        summary: &TeamMemberSummary,
    ) -> Result<(), RuntimeError> {
        let conn = self.open()?;
        conn.execute(
            r#"
            INSERT INTO team_members (team_dir, name, summary_json)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(team_dir, name) DO UPDATE SET summary_json = excluded.summary_json
            "#,
            params![Self::team_key(team_dir), summary.name, to_json(summary)?],
        )
        .map_err(sqlite_error)?;
        Ok(())
    }

    fn read_team_inbox(
        &self,
        team_dir: &Path,
        agent_name: &str,
    ) -> Result<Vec<TeamMessage>, RuntimeError> {
        let mut conn = self.open()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        let team_key = Self::team_key(team_dir);
        let ids_and_payloads = {
            let mut stmt = tx
                .prepare(
                    "SELECT id, payload_json FROM team_inbox WHERE team_dir = ?1 AND recipient = ?2 AND delivery_state = ?3 ORDER BY created_at, id",
                )
                .map_err(sqlite_error)?;
            stmt.query_map(params![team_key, agent_name, DELIVERY_PENDING], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sqlite_error)?
        };

        for (id, _) in &ids_and_payloads {
            tx.execute(
                "UPDATE team_inbox SET delivery_state = ?2 WHERE id = ?1",
                params![id, DELIVERY_INFLIGHT],
            )
            .map_err(sqlite_error)?;
        }
        tx.commit().map_err(sqlite_error)?;

        ids_and_payloads
            .into_iter()
            .map(|(_, payload)| from_json(&payload))
            .collect()
    }

    fn ack_team_inbox(&self, team_dir: &Path, agent_name: &str) -> Result<(), RuntimeError> {
        let conn = self.open()?;
        conn.execute(
            "UPDATE team_inbox SET delivery_state = ?3 WHERE team_dir = ?1 AND recipient = ?2 AND delivery_state = ?4",
            params![Self::team_key(team_dir), agent_name, DELIVERY_ACKED, DELIVERY_INFLIGHT],
        )
        .map_err(sqlite_error)?;
        Ok(())
    }

    fn requeue_team_inbox(&self, team_dir: &Path, agent_name: &str) -> Result<(), RuntimeError> {
        let conn = self.open()?;
        conn.execute(
            "UPDATE team_inbox SET delivery_state = ?3 WHERE team_dir = ?1 AND recipient = ?2 AND delivery_state = ?4",
            params![Self::team_key(team_dir), agent_name, DELIVERY_PENDING, DELIVERY_INFLIGHT],
        )
        .map_err(sqlite_error)?;
        Ok(())
    }

    fn append_team_message(
        &self,
        team_dir: &Path,
        recipient: &str,
        message: &TeamMessage,
    ) -> Result<(), RuntimeError> {
        let conn = self.open()?;
        conn.execute(
            "INSERT INTO team_inbox (id, team_dir, recipient, payload_json, delivery_state, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                next_id("teammsg"),
                Self::team_key(team_dir),
                recipient,
                to_json(message)?,
                DELIVERY_PENDING,
                now_secs(),
            ],
        )
        .map_err(sqlite_error)?;
        Ok(())
    }

    fn load_team_requests(
        &self,
        team_dir: &Path,
    ) -> Result<Vec<TeamProtocolRequestSummary>, RuntimeError> {
        let conn = self.open()?;
        let mut stmt = conn
            .prepare(
                "SELECT payload_json FROM team_requests WHERE team_dir = ?1 ORDER BY created_at, request_id",
            )
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(params![Self::team_key(team_dir)], |row| {
                row.get::<_, String>(0)
            })
            .map_err(sqlite_error)?;
        let mut requests = Vec::new();
        for row in rows {
            requests.push(from_json(&row.map_err(sqlite_error)?)?);
        }
        Ok(requests)
    }

    fn upsert_team_request(
        &self,
        team_dir: &Path,
        request: &TeamProtocolRequestSummary,
    ) -> Result<(), RuntimeError> {
        let conn = self.open()?;
        conn.execute(
            r#"
            INSERT INTO team_requests (request_id, team_dir, payload_json, created_at)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(request_id) DO UPDATE SET
                team_dir = excluded.team_dir,
                payload_json = excluded.payload_json
            "#,
            params![
                request.request_id,
                Self::team_key(team_dir),
                to_json(request)?,
                request.created_at as i64,
            ],
        )
        .map_err(sqlite_error)?;
        Ok(())
    }

    fn list_team_agent_names(&self, team_dir: &Path) -> Result<Vec<String>, RuntimeError> {
        let conn = self.open()?;
        let mut stmt = conn
            .prepare("SELECT name FROM agents WHERE team_dir = ?1 ORDER BY name")
            .map_err(sqlite_error)?;
        stmt.query_map(params![Self::team_key(team_dir)], |row| {
            row.get::<_, String>(0)
        })
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)
    }
}

impl BackgroundStore for SqliteRuntimeStore {
    fn load_background_tasks(
        &self,
        agent_id: &str,
    ) -> Result<Vec<BackgroundTaskSummary>, RuntimeError> {
        let conn = self.open()?;
        let mut stmt = conn
            .prepare(
                "SELECT payload_json FROM background_jobs WHERE agent_id = ?1 ORDER BY created_at, id",
            )
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(params![agent_id], |row| row.get::<_, String>(0))
            .map_err(sqlite_error)?;
        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(from_json(&row.map_err(sqlite_error)?)?);
        }
        Ok(tasks)
    }

    fn upsert_background_task(
        &self,
        agent_id: &str,
        task: &BackgroundTaskSummary,
        notification_state: i64,
    ) -> Result<(), RuntimeError> {
        let conn = self.open()?;
        conn.execute(
            r#"
            INSERT INTO background_jobs (id, agent_id, payload_json, notification_state, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?5)
            ON CONFLICT(id) DO UPDATE SET
                agent_id = excluded.agent_id,
                payload_json = excluded.payload_json,
                notification_state = excluded.notification_state,
                updated_at = excluded.updated_at
            "#,
            params![task.id, agent_id, to_json(task)?, notification_state, now_secs()],
        )
        .map_err(sqlite_error)?;
        Ok(())
    }

    fn drain_background_notifications(
        &self,
        agent_id: &str,
    ) -> Result<Vec<BackgroundNotification>, RuntimeError> {
        let mut conn = self.open()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        let jobs = {
            let mut stmt = tx
                .prepare(
                    "SELECT id, payload_json FROM background_jobs WHERE agent_id = ?1 AND notification_state = ?2 ORDER BY updated_at, id",
                )
                .map_err(sqlite_error)?;
            stmt.query_map(params![agent_id, DELIVERY_PENDING], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sqlite_error)?
        };
        for (id, _) in &jobs {
            tx.execute(
                "UPDATE background_jobs SET notification_state = ?2 WHERE id = ?1",
                params![id, DELIVERY_INFLIGHT],
            )
            .map_err(sqlite_error)?;
        }
        tx.commit().map_err(sqlite_error)?;

        jobs.into_iter()
            .map(|(_, payload)| {
                let task: BackgroundTaskSummary = from_json(&payload)?;
                Ok(BackgroundNotification {
                    task_id: task.id,
                    command: task.command,
                    cwd: task.cwd,
                    status: task.status,
                    output_preview: task
                        .output_preview
                        .unwrap_or_else(|| "(no output)".to_string()),
                })
            })
            .collect()
    }

    fn ack_background_notifications(&self, agent_id: &str) -> Result<(), RuntimeError> {
        let conn = self.open()?;
        conn.execute(
            "UPDATE background_jobs SET notification_state = ?2 WHERE agent_id = ?1 AND notification_state = ?3",
            params![agent_id, DELIVERY_ACKED, DELIVERY_INFLIGHT],
        )
        .map_err(sqlite_error)?;
        Ok(())
    }

    fn requeue_background_notifications(&self, agent_id: &str) -> Result<(), RuntimeError> {
        let conn = self.open()?;
        conn.execute(
            "UPDATE background_jobs SET notification_state = ?2 WHERE agent_id = ?1 AND notification_state = ?3",
            params![agent_id, DELIVERY_PENDING, DELIVERY_INFLIGHT],
        )
        .map_err(sqlite_error)?;
        Ok(())
    }
}

#[derive(Clone)]
/// SQLite-backed [`RuntimeStore`] implementation used by default.
pub struct SqliteRuntimeStore {
    path: PathBuf,
}

impl Default for SqliteRuntimeStore {
    fn default() -> Self {
        Self::new(Self::default_path())
    }
}

impl SqliteRuntimeStore {
    /// Returns the default SQLite path used when no explicit store path is provided.
    pub fn default_path() -> PathBuf {
        default_store_dir().join("runtime.sqlite")
    }

    /// Returns the default directory used for Mentra runtime stores.
    pub fn default_directory() -> PathBuf {
        default_store_dir()
    }

    /// Creates a SQLite runtime store in the default directory using a runtime-scoped filename.
    pub fn for_runtime_identifier(runtime_identifier: &str) -> Self {
        Self::new(Self::path_for_runtime_identifier(runtime_identifier))
    }

    /// Returns the default SQLite path for a specific runtime identifier.
    pub fn path_for_runtime_identifier(runtime_identifier: &str) -> PathBuf {
        Self::default_directory().join(format!(
            "runtime-{}.sqlite",
            encode_runtime_identifier(runtime_identifier)
        ))
    }

    /// Lists runtime identifiers that have persisted SQLite stores in the default directory.
    pub fn list_persisted_runtime_identifiers() -> Result<Vec<String>, RuntimeError> {
        let base = Self::default_directory();
        let Ok(entries) = std::fs::read_dir(&base) else {
            return Ok(Vec::new());
        };

        let mut runtime_identifiers = entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter_map(|filename| decode_runtime_store_filename(&filename))
            .collect::<Vec<_>>();
        runtime_identifiers.sort();
        runtime_identifiers.dedup();
        Ok(runtime_identifiers)
    }

    /// Creates a SQLite runtime store at the provided path.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Returns the SQLite database path for the store.
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
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(sqlite_error)?;
        self.ensure_schema(&conn)?;
        Ok(conn)
    }

    fn ensure_schema(&self, conn: &Connection) -> Result<(), RuntimeError> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS agents (
                id TEXT PRIMARY KEY,
                runtime_identifier TEXT NOT NULL,
                name TEXT NOT NULL,
                model TEXT NOT NULL,
                provider_id TEXT NOT NULL,
                team_dir TEXT NOT NULL,
                tasks_namespace TEXT NOT NULL,
                is_teammate INTEGER NOT NULL,
                config_json TEXT NOT NULL,
                hidden_tools_json TEXT NOT NULL,
                max_rounds INTEGER,
                teammate_identity_json TEXT,
                rounds_since_task INTEGER NOT NULL,
                idle_requested INTEGER NOT NULL,
                status_json TEXT NOT NULL,
                subagents_json TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS agent_memory (
                agent_id TEXT PRIMARY KEY,
                revision INTEGER NOT NULL,
                state_json TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS agent_runs (
                id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                state TEXT NOT NULL,
                error TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS tasks (
                namespace TEXT NOT NULL,
                id INTEGER NOT NULL,
                payload_json TEXT NOT NULL,
                PRIMARY KEY (namespace, id)
            );
            CREATE TABLE IF NOT EXISTS task_edges (
                namespace TEXT NOT NULL,
                blocker_id INTEGER NOT NULL,
                dependent_id INTEGER NOT NULL,
                PRIMARY KEY (namespace, blocker_id, dependent_id)
            );
            CREATE TABLE IF NOT EXISTS team_members (
                team_dir TEXT NOT NULL,
                name TEXT NOT NULL,
                summary_json TEXT NOT NULL,
                PRIMARY KEY (team_dir, name)
            );
            CREATE TABLE IF NOT EXISTS team_inbox (
                id TEXT PRIMARY KEY,
                team_dir TEXT NOT NULL,
                recipient TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                delivery_state INTEGER NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS team_requests (
                request_id TEXT PRIMARY KEY,
                team_dir TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS background_jobs (
                id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                notification_state INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS audit_events (
                id TEXT PRIMARY KEY,
                scope TEXT NOT NULL,
                event_type TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS leases (
                key TEXT PRIMARY KEY,
                owner TEXT NOT NULL,
                expires_at INTEGER NOT NULL
            );
            "#,
        )
        .map_err(sqlite_error)
    }

    fn write_agent(
        &self,
        conn: &Connection,
        record: &PersistedAgentRecord,
    ) -> Result<(), RuntimeError> {
        let now = now_secs();
        conn.execute(
            r#"
            INSERT INTO agents (
                id, runtime_identifier, name, model, provider_id, team_dir, tasks_namespace, is_teammate, config_json,
                hidden_tools_json, max_rounds, teammate_identity_json, rounds_since_task,
                idle_requested, status_json, subagents_json, created_at, updated_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
            ON CONFLICT(id) DO UPDATE SET
                runtime_identifier = excluded.runtime_identifier,
                name = excluded.name,
                model = excluded.model,
                provider_id = excluded.provider_id,
                team_dir = excluded.team_dir,
                tasks_namespace = excluded.tasks_namespace,
                is_teammate = excluded.is_teammate,
                config_json = excluded.config_json,
                hidden_tools_json = excluded.hidden_tools_json,
                max_rounds = excluded.max_rounds,
                teammate_identity_json = excluded.teammate_identity_json,
                rounds_since_task = excluded.rounds_since_task,
                idle_requested = excluded.idle_requested,
                status_json = excluded.status_json,
                subagents_json = excluded.subagents_json,
                updated_at = excluded.updated_at
            "#,
            params![
                record.id,
                record.runtime_identifier,
                record.name,
                record.model,
                record.provider_id.as_str(),
                record.config.team.team_dir.to_string_lossy().into_owned(),
                record.config.task.tasks_dir.to_string_lossy().into_owned(),
                i64::from(record.teammate_identity.is_some()),
                to_json(&record.config)?,
                to_json(&record.hidden_tools)?,
                record.max_rounds.map(|value| value as i64),
                maybe_json(&record.teammate_identity)?,
                record.rounds_since_task as i64,
                i64::from(record.idle_requested),
                to_json(&record.status)?,
                to_json(&record.subagents)?,
                now,
                now,
            ],
        )
        .map_err(sqlite_error)?;
        Ok(())
    }

    fn write_agent_memory(
        &self,
        conn: &Connection,
        agent_id: &str,
        memory: &AgentMemoryState,
    ) -> Result<(), RuntimeError> {
        conn.execute(
            r#"
            INSERT INTO agent_memory (agent_id, revision, state_json, updated_at)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(agent_id) DO UPDATE SET
                revision = excluded.revision,
                state_json = excluded.state_json,
                updated_at = excluded.updated_at
            "#,
            params![
                agent_id,
                memory.revision as i64,
                to_json(memory)?,
                now_secs()
            ],
        )
        .map_err(sqlite_error)?;
        Ok(())
    }

    fn team_key(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    fn task_namespace(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }
}

impl RuntimeStore for SqliteRuntimeStore {
    fn prepare_recovery(&self) -> Result<(), RuntimeError> {
        let mut conn = self.open()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;

        tx.execute(
            "UPDATE team_inbox SET delivery_state = ?1 WHERE delivery_state = ?2",
            params![DELIVERY_PENDING, DELIVERY_INFLIGHT],
        )
        .map_err(sqlite_error)?;
        tx.execute(
            "UPDATE background_jobs SET notification_state = ?1 WHERE notification_state = ?2",
            params![DELIVERY_PENDING, DELIVERY_INFLIGHT],
        )
        .map_err(sqlite_error)?;

        {
            let mut stmt = tx
                .prepare("SELECT id, payload_json FROM background_jobs")
                .map_err(sqlite_error)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(sqlite_error)?;
            for row in rows {
                let (id, payload) = row.map_err(sqlite_error)?;
                let mut task: BackgroundTaskSummary = from_json(&payload)?;
                if task.status == BackgroundTaskStatus::Running {
                    task.status = BackgroundTaskStatus::Interrupted;
                    tx.execute(
                        "UPDATE background_jobs SET payload_json = ?2, notification_state = ?3, updated_at = ?4 WHERE id = ?1",
                        params![id, to_json(&task)?, DELIVERY_PENDING, now_secs()],
                    )
                    .map_err(sqlite_error)?;
                }
            }
        }

        tx.execute(
            "DELETE FROM leases WHERE expires_at <= ?1",
            params![now_secs()],
        )
        .map_err(sqlite_error)?;
        prune_stale_runtime_leases(&tx)?;
        tx.commit().map_err(sqlite_error)
    }

    fn create_agent(
        &self,
        record: &PersistedAgentRecord,
        memory: &AgentMemoryState,
    ) -> Result<(), RuntimeError> {
        let mut conn = self.open()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        self.write_agent(&tx, record)?;
        self.write_agent_memory(&tx, &record.id, memory)?;
        tx.commit().map_err(sqlite_error)
    }

    fn save_agent_record(&self, record: &PersistedAgentRecord) -> Result<(), RuntimeError> {
        let mut conn = self.open()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        self.write_agent(&tx, record)?;
        tx.commit().map_err(sqlite_error)
    }

    fn save_agent_memory(
        &self,
        agent_id: &str,
        memory: &AgentMemoryState,
    ) -> Result<(), RuntimeError> {
        let mut conn = self.open()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        self.write_agent_memory(&tx, agent_id, memory)?;
        tx.commit().map_err(sqlite_error)
    }

    fn load_agent(&self, agent_id: &str) -> Result<Option<LoadedAgentState>, RuntimeError> {
        let conn = self.open()?;
        let record = conn
            .query_row(
                r#"
                SELECT
                    id, runtime_identifier, name, model, provider_id, config_json,
                    hidden_tools_json, max_rounds, teammate_identity_json, rounds_since_task,
                    idle_requested, status_json, subagents_json
                FROM agents WHERE id = ?1
                "#,
                params![agent_id],
                |row| {
                    let provider_id: String = row.get(4)?;
                    let config_json: String = row.get(5)?;
                    let hidden_tools_json: String = row.get(6)?;
                    let teammate_identity_json: Option<String> = row.get(8)?;
                    let status_json: String = row.get(11)?;
                    let subagents_json: String = row.get(12)?;
                    Ok(PersistedAgentRecord {
                        id: row.get(0)?,
                        runtime_identifier: row.get(1)?,
                        name: row.get(2)?,
                        model: row.get(3)?,
                        provider_id: ProviderId::from(provider_id),
                        config: from_json(&config_json).map_err(to_sql_error)?,
                        hidden_tools: from_json(&hidden_tools_json).map_err(to_sql_error)?,
                        max_rounds: row.get::<_, Option<i64>>(7)?.map(|value| value as usize),
                        teammate_identity: teammate_identity_json
                            .map(|json| from_json(&json))
                            .transpose()
                            .map_err(to_sql_error)?,
                        rounds_since_task: row.get::<_, i64>(9)? as usize,
                        idle_requested: row.get::<_, i64>(10)? != 0,
                        status: from_json(&status_json).map_err(to_sql_error)?,
                        subagents: from_json(&subagents_json).map_err(to_sql_error)?,
                    })
                },
            )
            .optional()
            .map_err(sqlite_error)?;
        let Some(record) = record else {
            return Ok(None);
        };

        let memory = conn
            .query_row(
                "SELECT state_json FROM agent_memory WHERE agent_id = ?1",
                params![agent_id],
                |row| {
                    let state_json: String = row.get(0)?;
                    from_json(&state_json).map_err(to_sql_error)
                },
            )
            .optional()
            .map_err(sqlite_error)?;
        let Some(memory) = memory else {
            return Err(RuntimeError::Store(format!(
                "Agent '{agent_id}' is missing persisted memory"
            )));
        };

        Ok(Some(LoadedAgentState { record, memory }))
    }

    fn list_agents(&self) -> Result<Vec<LoadedAgentState>, RuntimeError> {
        let conn = self.open()?;
        let mut stmt = conn
            .prepare("SELECT id FROM agents ORDER BY created_at, id")
            .map_err(sqlite_error)?;
        let ids = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sqlite_error)?;
        ids.into_iter()
            .map(|id| {
                self.load_agent(&id)?
                    .ok_or_else(|| RuntimeError::Store(format!("Agent '{id}' disappeared")))
            })
            .collect()
    }

    fn list_agents_by_runtime(
        &self,
        runtime_identifier: &str,
    ) -> Result<Vec<LoadedAgentState>, RuntimeError> {
        let conn = self.open()?;
        let mut stmt = conn
            .prepare("SELECT id FROM agents WHERE runtime_identifier = ?1 ORDER BY created_at, id")
            .map_err(sqlite_error)?;
        let ids = stmt
            .query_map(params![runtime_identifier], |row| row.get::<_, String>(0))
            .map_err(sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sqlite_error)?;
        ids.into_iter()
            .map(|id| {
                self.load_agent(&id)?
                    .ok_or_else(|| RuntimeError::Store(format!("Agent '{id}' disappeared")))
            })
            .collect()
    }

    fn start_run(&self, agent_id: &str) -> Result<String, RuntimeError> {
        let run_id = next_id("run");
        let conn = self.open()?;
        conn.execute(
            "INSERT INTO agent_runs (id, agent_id, state, error, created_at, updated_at) VALUES (?1, ?2, 'running', NULL, ?3, ?3)",
            params![run_id, agent_id, now_secs()],
        )
        .map_err(sqlite_error)?;
        Ok(run_id)
    }

    fn update_run_state(
        &self,
        run_id: &str,
        state: &str,
        error: Option<&str>,
    ) -> Result<(), RuntimeError> {
        let conn = self.open()?;
        conn.execute(
            "UPDATE agent_runs SET state = ?2, error = ?3, updated_at = ?4 WHERE id = ?1",
            params![run_id, state, error, now_secs()],
        )
        .map_err(sqlite_error)?;
        Ok(())
    }

    fn finish_run(&self, run_id: &str) -> Result<(), RuntimeError> {
        self.update_run_state(run_id, "finished", None)
    }

    fn fail_run(&self, run_id: &str, error: &str) -> Result<(), RuntimeError> {
        self.update_run_state(run_id, "failed", Some(error))
    }

    fn load_tasks(&self, namespace: &Path) -> Result<Vec<TaskItem>, RuntimeError> {
        let conn = self.open()?;
        self.load_tasks_from_conn(&conn, namespace)
    }

    fn capture_tasks(&self, namespace: &Path) -> Result<TaskStateSnapshot, RuntimeError> {
        Ok(TaskStateSnapshot {
            tasks: self.load_tasks(namespace)?,
        })
    }

    fn restore_tasks(
        &self,
        namespace: &Path,
        snapshot: &TaskStateSnapshot,
    ) -> Result<(), RuntimeError> {
        self.replace_tasks(namespace, &snapshot.tasks)
    }

    fn replace_tasks(&self, namespace: &Path, tasks: &[TaskItem]) -> Result<(), RuntimeError> {
        let mut conn = self.open()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        let namespace = Self::task_namespace(namespace);
        tx.execute(
            "DELETE FROM tasks WHERE namespace = ?1",
            params![namespace.clone()],
        )
        .map_err(sqlite_error)?;
        tx.execute(
            "DELETE FROM task_edges WHERE namespace = ?1",
            params![namespace.clone()],
        )
        .map_err(sqlite_error)?;
        for task in tasks {
            tx.execute(
                "INSERT INTO tasks (namespace, id, payload_json) VALUES (?1, ?2, ?3)",
                params![namespace.clone(), task.id as i64, to_json(task)?],
            )
            .map_err(sqlite_error)?;
            for blocker in &task.blocked_by {
                tx.execute(
                    "INSERT OR IGNORE INTO task_edges (namespace, blocker_id, dependent_id) VALUES (?1, ?2, ?3)",
                    params![namespace.clone(), *blocker as i64, task.id as i64],
                )
                .map_err(sqlite_error)?;
            }
        }
        tx.commit().map_err(sqlite_error)
    }

    fn record_audit_event(
        &self,
        scope: &str,
        event_type: &str,
        payload: serde_json::Value,
    ) -> Result<(), RuntimeError> {
        let conn = self.open()?;
        conn.execute(
            "INSERT INTO audit_events (id, scope, event_type, payload_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![next_id("audit"), scope, event_type, payload.to_string(), now_secs()],
        )
        .map_err(sqlite_error)?;
        Ok(())
    }

    fn acquire_lease(&self, key: &str, owner: &str, ttl: Duration) -> Result<bool, RuntimeError> {
        let mut conn = self.open()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        let now = now_secs();
        tx.execute("DELETE FROM leases WHERE expires_at <= ?1", params![now])
            .map_err(sqlite_error)?;
        prune_stale_runtime_leases(&tx)?;
        let inserted = tx
            .execute(
                "INSERT OR IGNORE INTO leases (key, owner, expires_at) VALUES (?1, ?2, ?3)",
                params![key, owner, now + ttl.as_secs() as i64],
            )
            .map_err(sqlite_error)?;
        tx.commit().map_err(sqlite_error)?;
        Ok(inserted == 1)
    }

    fn release_lease(&self, key: &str, owner: &str) -> Result<(), RuntimeError> {
        let conn = self.open()?;
        conn.execute(
            "DELETE FROM leases WHERE key = ?1 AND owner = ?2",
            params![key, owner],
        )
        .map_err(sqlite_error)?;
        Ok(())
    }
}

impl SqliteRuntimeStore {
    fn load_tasks_from_conn(
        &self,
        conn: &Connection,
        namespace: &Path,
    ) -> Result<Vec<TaskItem>, RuntimeError> {
        let mut stmt = conn
            .prepare("SELECT payload_json FROM tasks WHERE namespace = ?1 ORDER BY id")
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(params![Self::task_namespace(namespace)], |row| {
                row.get::<_, String>(0)
            })
            .map_err(sqlite_error)?;
        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(from_json(&row.map_err(sqlite_error)?)?);
        }
        Ok(tasks)
    }
}

fn to_json<T: Serialize>(value: &T) -> Result<String, RuntimeError> {
    serde_json::to_string(value).map_err(|error| RuntimeError::Store(error.to_string()))
}

fn maybe_json<T: Serialize>(value: &Option<T>) -> Result<Option<String>, RuntimeError> {
    value.as_ref().map(to_json).transpose()
}

fn from_json<T: DeserializeOwned>(value: &str) -> Result<T, RuntimeError> {
    serde_json::from_str(value).map_err(|error| RuntimeError::Store(error.to_string()))
}

fn sqlite_error(error: rusqlite::Error) -> RuntimeError {
    RuntimeError::Store(error.to_string())
}

fn to_sql_error(error: RuntimeError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::other(error.to_string())),
    )
}

fn next_id(prefix: &str) -> String {
    let counter = NEXT_STORE_ID.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{:x}-{:x}", now_nanos(), counter)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn prune_stale_runtime_leases(tx: &rusqlite::Transaction<'_>) -> Result<(), RuntimeError> {
    let mut stmt = tx
        .prepare("SELECT key, owner FROM leases")
        .map_err(sqlite_error)?;
    let leases = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?;
    drop(stmt);

    for (key, owner) in leases {
        if runtime_owner_is_stale(&owner) {
            tx.execute("DELETE FROM leases WHERE key = ?1", params![key])
                .map_err(sqlite_error)?;
        }
    }

    Ok(())
}

fn runtime_owner_is_stale(owner: &str) -> bool {
    let Some(pid) = owner
        .strip_prefix("runtime-")
        .and_then(|value| value.parse::<i32>().ok())
    else {
        return false;
    };

    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(pid, 0) };
        if result == 0 {
            return false;
        }

        match std::io::Error::last_os_error().raw_os_error() {
            Some(code) if code == libc::ESRCH => true,
            Some(code) if code == libc::EPERM => false,
            _ => false,
        }
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn encode_runtime_identifier(runtime_identifier: &str) -> String {
    let mut encoded = String::with_capacity(runtime_identifier.len() * 2);
    for byte in runtime_identifier.as_bytes() {
        use std::fmt::Write as _;
        let _ = write!(&mut encoded, "{byte:02x}");
    }
    encoded
}

fn decode_runtime_store_filename(filename: &str) -> Option<String> {
    let encoded = filename.strip_prefix("runtime-")?.strip_suffix(".sqlite")?;
    if encoded.len() % 2 != 0 || encoded.is_empty() {
        return None;
    }

    let mut bytes = Vec::with_capacity(encoded.len() / 2);
    let mut index = 0;
    while index < encoded.len() {
        let byte = u8::from_str_radix(&encoded[index..index + 2], 16).ok()?;
        bytes.push(byte);
        index += 2;
    }
    String::from_utf8(bytes).ok()
}

#[cfg(not(test))]
fn default_store_dir() -> PathBuf {
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&workspace, &mut hasher);
    let workspace_key = std::hash::Hasher::finish(&hasher);
    std::env::temp_dir()
        .join("mentra-runtime")
        .join(format!("workspace-{workspace_key:016x}"))
}

#[cfg(test)]
fn default_store_dir() -> PathBuf {
    let suffix = NEXT_TEST_STORE_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir()
        .join("mentra-test-runtime")
        .join(format!("process-{}-{suffix}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_identifier_round_trips_through_filename_encoding() {
        let runtime_identifier = "chat/example 01";
        let filename = format!(
            "runtime-{}.sqlite",
            encode_runtime_identifier(runtime_identifier)
        );
        assert_eq!(
            decode_runtime_store_filename(&filename).as_deref(),
            Some(runtime_identifier)
        );
    }

    #[test]
    fn path_for_runtime_identifier_uses_runtime_specific_filename() {
        let path = SqliteRuntimeStore::path_for_runtime_identifier("session-a");
        assert!(
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("runtime-"))
        );
        assert!(
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".sqlite"))
        );
    }

    #[test]
    fn stale_runtime_owner_can_be_reclaimed() {
        let store = SqliteRuntimeStore::new(
            std::env::temp_dir().join(format!("mentra-store-lease-{}.sqlite", now_nanos())),
        );
        let conn = Connection::open(store.path()).expect("open store");
        store.ensure_schema(&conn).expect("ensure schema");
        conn.execute(
            "INSERT INTO leases (key, owner, expires_at) VALUES (?1, ?2, ?3)",
            params!["agent:test", "runtime-999999", now_secs() + 3600],
        )
        .expect("insert stale lease");

        let acquired = store
            .acquire_lease("agent:test", "runtime-123", Duration::from_secs(60))
            .expect("acquire lease");
        assert!(acquired);
    }
}
