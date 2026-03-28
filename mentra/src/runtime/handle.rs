mod agents;
mod construction;
mod execution;
mod tooling;

use std::{
    any::{Any, TypeId},
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use tokio::sync::{broadcast, watch};

use crate::{
    agent::{AgentEvent, AgentSnapshot},
    background::{BackgroundNotification, BackgroundTaskManager, BackgroundTaskSummary},
    compaction::CompactionEngine,
    memory::MemoryEngine,
    runtime::{
        control::{
            AuditHook, CommandOutput, CommandRequest, CommandSpec, LocalRuntimeExecutor,
            RuntimeExecutor, RuntimeHookEvent, RuntimeHooks, RuntimePolicy, read_limited_file,
        },
        error::RuntimeError,
        store::{RuntimeStore, SqliteRuntimeStore},
        task::{self, TaskAccess},
    },
    team::{
        TeamDispatch, TeamManager, TeamMemberSummary, TeamMessage, TeamProtocolRequestSummary,
        TeamRequestFilter, TeammateHost,
    },
    tool::{ExecutableTool, ToolAuthorizer, ToolRegistry},
};

use super::skill::SkillLoader;

#[derive(Clone)]
pub struct RuntimeHandle {
    pub(crate) execution: ExecutionServices,
    pub(crate) persistence: PersistenceServices,
    pub(crate) collaboration: CollaborationServices,
    pub(crate) tooling: ToolingServices,
    pub(crate) runtime_intrinsics_enabled: bool,
    runtime_instance_id: String,
    persisted_runtime_identifier: Arc<str>,
    lease_keys: Arc<Mutex<BTreeSet<String>>>,
    agent_contexts: Arc<RwLock<HashMap<String, AgentExecutionConfig>>>,
}

#[derive(Clone)]
pub(crate) struct ExecutionServices {
    pub(crate) executor: Arc<dyn RuntimeExecutor>,
    pub(crate) policy: Arc<RuntimePolicy>,
    pub(crate) tool_authorizer: Option<Arc<dyn ToolAuthorizer>>,
    pub(crate) hooks: RuntimeHooks,
}

#[derive(Clone)]
pub(crate) struct PersistenceServices {
    pub(crate) store: Arc<dyn RuntimeStore>,
    pub(crate) memory: Arc<MemoryEngine>,
    pub(crate) compaction: Arc<dyn CompactionEngine>,
}

#[derive(Clone)]
pub(crate) struct CollaborationServices {
    pub(crate) background_tasks: BackgroundTaskManager,
    pub(crate) team: TeamManager,
    pub(crate) teammate_host: TeammateHost,
}

#[derive(Clone)]
pub(crate) struct ToolingServices {
    pub(crate) tool_registry: Arc<RwLock<ToolRegistry>>,
    pub(crate) skill_loader: Arc<RwLock<Option<SkillLoader>>>,
    pub(crate) app_contexts: Arc<RwLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>>,
}

#[derive(Clone)]
pub(crate) struct AgentObserver {
    pub(crate) events: broadcast::Sender<AgentEvent>,
    pub(crate) snapshot_tx: watch::Sender<AgentSnapshot>,
    pub(crate) snapshot: Arc<Mutex<AgentSnapshot>>,
}

#[derive(Debug, Clone)]
pub(crate) struct AgentExecutionConfig {
    pub(crate) name: String,
    pub(crate) team_dir: PathBuf,
    pub(crate) tasks_dir: PathBuf,
    pub(crate) base_dir: PathBuf,
    pub(crate) memory_tool_search_limit: usize,
    pub(crate) auto_route_shell: bool,
    pub(crate) is_teammate: bool,
}

impl Drop for RuntimeHandle {
    fn drop(&mut self) {
        if Arc::strong_count(&self.lease_keys) != 1 {
            return;
        }

        let lease_keys = {
            let lease_keys = self.lease_keys.lock().expect("lease key registry poisoned");
            lease_keys.iter().cloned().collect::<Vec<_>>()
        };

        for key in lease_keys {
            let _ = self
                .persistence
                .store
                .release_lease(&key, &self.runtime_instance_id);
        }
    }
}

impl RuntimeHandle {
    pub(crate) fn memory_engine(&self) -> Arc<MemoryEngine> {
        self.persistence.memory.clone()
    }

    pub(crate) fn compaction_engine(&self) -> Arc<dyn CompactionEngine> {
        self.persistence.compaction.clone()
    }
}
