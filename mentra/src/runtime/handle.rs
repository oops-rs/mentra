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

use tokio::sync::watch;

use crate::{
    agent::{AgentEventBus, AgentSnapshot},
    background::{BackgroundNotification, BackgroundTaskManager, BackgroundTaskSummary},
    compaction::CompactionEngine,
    memory::MemoryEngine,
    provider::{Provider, ProviderId, ProviderRegistry},
    runtime::{
        control::{
            AuditHook, CommandOutput, CommandRequest, CommandSpec, LocalRuntimeExecutor,
            PreExecutionHooks, RuntimeExecutor, RuntimeHookEvent, RuntimeHooks, RuntimePolicy,
            read_limited_file,
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
    provider_registry: Arc<RwLock<ProviderRegistry>>,
}

#[derive(Clone)]
pub(crate) struct ExecutionServices {
    pub(crate) executor: Arc<dyn RuntimeExecutor>,
    pub(crate) policy: Arc<RuntimePolicy>,
    pub(crate) tool_authorizer: Option<Arc<dyn ToolAuthorizer>>,
    pub(crate) hooks: RuntimeHooks,
    pub(crate) pre_hooks: PreExecutionHooks,
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
    pub(crate) events: AgentEventBus,
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
    pub(crate) fn get_provider(&self, id: Option<&ProviderId>) -> Option<Arc<dyn Provider>> {
        self.provider_registry
            .read()
            .expect("provider registry poisoned")
            .get_provider(id)
    }

    pub(crate) fn memory_engine(&self) -> Arc<MemoryEngine> {
        self.persistence.memory.clone()
    }

    pub(crate) fn compaction_engine(&self) -> Arc<dyn CompactionEngine> {
        self.persistence.compaction.clone()
    }

    pub(crate) fn pre_hooks(&self) -> &PreExecutionHooks {
        &self.execution.pre_hooks
    }

    pub(crate) fn with_provider_registry(
        &self,
        provider_registry: Arc<RwLock<ProviderRegistry>>,
    ) -> Self {
        Self {
            execution: self.execution.clone(),
            persistence: self.persistence.clone(),
            collaboration: self.collaboration.clone(),
            tooling: self.tooling.clone(),
            runtime_intrinsics_enabled: self.runtime_intrinsics_enabled,
            runtime_instance_id: self.runtime_instance_id.clone(),
            persisted_runtime_identifier: self.persisted_runtime_identifier.clone(),
            lease_keys: self.lease_keys.clone(),
            agent_contexts: self.agent_contexts.clone(),
            provider_registry,
        }
    }
}
