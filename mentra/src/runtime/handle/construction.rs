use super::*;
use crate::background::BackgroundHookSink;
use crate::memory::MemoryEngine;

#[derive(Clone)]
struct RuntimeBackgroundHookSink {
    store: Arc<dyn RuntimeStore>,
    hooks: RuntimeHooks,
}

impl BackgroundHookSink for RuntimeBackgroundHookSink {
    fn task_started(
        &self,
        agent_id: &str,
        task_id: &str,
        command: &str,
        cwd: &Path,
    ) -> Result<(), RuntimeError> {
        self.hooks.emit(
            self.store.as_ref(),
            &RuntimeHookEvent::BackgroundTaskStarted {
                agent_id: agent_id.to_string(),
                task_id: task_id.to_string(),
                command: command.to_string(),
                cwd: cwd.to_path_buf(),
            },
        )
    }

    fn task_finished(
        &self,
        agent_id: &str,
        task_id: &str,
        status: &str,
    ) -> Result<(), RuntimeError> {
        self.hooks.emit(
            self.store.as_ref(),
            &RuntimeHookEvent::BackgroundTaskFinished {
                agent_id: agent_id.to_string(),
                task_id: task_id.to_string(),
                status: status.to_string(),
            },
        )
    }
}

fn background_hook_sink(
    store: Arc<dyn RuntimeStore>,
    hooks: RuntimeHooks,
) -> Arc<dyn BackgroundHookSink> {
    Arc::new(RuntimeBackgroundHookSink { store, hooks })
}

fn clone_tooling_services(tooling: &ToolingServices) -> ToolingServices {
    ToolingServices {
        tool_registry: Arc::new(RwLock::new(
            tooling
                .tool_registry
                .read()
                .expect("tool registry poisoned")
                .clone(),
        )),
        skill_loader: Arc::new(RwLock::new(
            tooling
                .skill_loader
                .read()
                .expect("skill loader poisoned")
                .clone(),
        )),
        app_contexts: tooling.app_contexts.clone(),
    }
}

impl RuntimeHandle {
    pub fn new(runtime_intrinsics_enabled: bool) -> Self {
        Self::with_components(
            Arc::new(SqliteRuntimeStore::default()),
            Arc::new(LocalRuntimeExecutor),
            Arc::new(RuntimePolicy::default()),
            RuntimeHooks::new().with_hook(AuditHook),
            runtime_intrinsics_enabled,
            Arc::<str>::from("default"),
        )
    }

    fn with_components(
        store: Arc<dyn RuntimeStore>,
        executor: Arc<dyn RuntimeExecutor>,
        policy: Arc<RuntimePolicy>,
        hooks: RuntimeHooks,
        runtime_intrinsics_enabled: bool,
        persisted_runtime_identifier: Arc<str>,
    ) -> Self {
        let _ = store.prepare_recovery();
        let runtime_instance_id = format!("runtime-{}", std::process::id());
        let memory = Arc::new(MemoryEngine::new(store.clone(), hooks.clone()));
        let mut tool_registry = ToolRegistry::default();
        if runtime_intrinsics_enabled {
            crate::runtime::intrinsic::register_tools(&mut tool_registry);
            tool_registry.register_builtin_tools();
        }
        let handle = Self {
            execution: ExecutionServices {
                executor: executor.clone(),
                policy,
                hooks: hooks.clone(),
            },
            persistence: PersistenceServices {
                store: store.clone(),
                memory,
            },
            collaboration: CollaborationServices {
                background_tasks: BackgroundTaskManager::new(
                    store.clone(),
                    executor,
                    background_hook_sink(store.clone(), hooks),
                ),
                team: TeamManager::new(store),
                teammate_host: TeammateHost::new().expect("teammate host"),
            },
            tooling: ToolingServices {
                tool_registry: Arc::new(RwLock::new(tool_registry)),
                skill_loader: Arc::new(RwLock::new(None)),
                app_contexts: Arc::new(RwLock::new(HashMap::new())),
            },
            runtime_intrinsics_enabled,
            runtime_instance_id,
            persisted_runtime_identifier,
            lease_keys: Arc::new(Mutex::new(BTreeSet::new())),
            agent_contexts: Arc::new(RwLock::new(HashMap::new())),
        };
        let _ = handle.emit_hook(RuntimeHookEvent::RecoveryPrepared {
            runtime_instance_id: handle.runtime_instance_id.clone(),
        });
        handle
    }

    pub fn rebind_store(&self, store: Arc<dyn RuntimeStore>) -> Self {
        let _ = store.prepare_recovery();
        let handle = Self {
            execution: self.execution.clone(),
            persistence: PersistenceServices {
                store: store.clone(),
                memory: Arc::new(MemoryEngine::new(
                    store.clone(),
                    self.execution.hooks.clone(),
                )),
            },
            collaboration: CollaborationServices {
                background_tasks: BackgroundTaskManager::new(
                    store.clone(),
                    self.execution.executor.clone(),
                    background_hook_sink(store.clone(), self.execution.hooks.clone()),
                ),
                team: TeamManager::new(store),
                teammate_host: self.collaboration.teammate_host.clone(),
            },
            tooling: clone_tooling_services(&self.tooling),
            runtime_intrinsics_enabled: self.runtime_intrinsics_enabled,
            runtime_instance_id: format!("runtime-{}", std::process::id()),
            persisted_runtime_identifier: self.persisted_runtime_identifier.clone(),
            lease_keys: Arc::new(Mutex::new(BTreeSet::new())),
            agent_contexts: Arc::new(RwLock::new(HashMap::new())),
        };
        let _ = handle.emit_hook(RuntimeHookEvent::RecoveryPrepared {
            runtime_instance_id: handle.runtime_instance_id.clone(),
        });
        handle
    }

    pub fn with_executor(&self, executor: Arc<dyn RuntimeExecutor>) -> Self {
        Self {
            execution: ExecutionServices {
                executor: executor.clone(),
                policy: self.execution.policy.clone(),
                hooks: self.execution.hooks.clone(),
            },
            persistence: PersistenceServices {
                store: self.persistence.store.clone(),
                memory: Arc::new(MemoryEngine::new(
                    self.persistence.store.clone(),
                    self.execution.hooks.clone(),
                )),
            },
            collaboration: CollaborationServices {
                background_tasks: BackgroundTaskManager::new(
                    self.persistence.store.clone(),
                    executor,
                    background_hook_sink(
                        self.persistence.store.clone(),
                        self.execution.hooks.clone(),
                    ),
                ),
                team: self.collaboration.team.clone(),
                teammate_host: self.collaboration.teammate_host.clone(),
            },
            tooling: clone_tooling_services(&self.tooling),
            runtime_intrinsics_enabled: self.runtime_intrinsics_enabled,
            runtime_instance_id: format!("runtime-{}", std::process::id()),
            persisted_runtime_identifier: self.persisted_runtime_identifier.clone(),
            lease_keys: Arc::new(Mutex::new(BTreeSet::new())),
            agent_contexts: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn with_policy(&self, policy: RuntimePolicy) -> Self {
        Self {
            execution: ExecutionServices {
                executor: self.execution.executor.clone(),
                policy: Arc::new(policy),
                hooks: self.execution.hooks.clone(),
            },
            persistence: PersistenceServices {
                store: self.persistence.store.clone(),
                memory: Arc::new(MemoryEngine::new(
                    self.persistence.store.clone(),
                    self.execution.hooks.clone(),
                )),
            },
            collaboration: CollaborationServices {
                background_tasks: BackgroundTaskManager::new(
                    self.persistence.store.clone(),
                    self.execution.executor.clone(),
                    background_hook_sink(
                        self.persistence.store.clone(),
                        self.execution.hooks.clone(),
                    ),
                ),
                team: self.collaboration.team.clone(),
                teammate_host: self.collaboration.teammate_host.clone(),
            },
            tooling: clone_tooling_services(&self.tooling),
            runtime_intrinsics_enabled: self.runtime_intrinsics_enabled,
            runtime_instance_id: format!("runtime-{}", std::process::id()),
            persisted_runtime_identifier: self.persisted_runtime_identifier.clone(),
            lease_keys: Arc::new(Mutex::new(BTreeSet::new())),
            agent_contexts: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn with_hooks(&self, hooks: RuntimeHooks) -> Self {
        Self {
            execution: ExecutionServices {
                executor: self.execution.executor.clone(),
                policy: self.execution.policy.clone(),
                hooks: hooks.clone(),
            },
            persistence: PersistenceServices {
                store: self.persistence.store.clone(),
                memory: Arc::new(MemoryEngine::new(
                    self.persistence.store.clone(),
                    hooks.clone(),
                )),
            },
            collaboration: CollaborationServices {
                background_tasks: BackgroundTaskManager::new(
                    self.persistence.store.clone(),
                    self.execution.executor.clone(),
                    background_hook_sink(self.persistence.store.clone(), hooks),
                ),
                team: self.collaboration.team.clone(),
                teammate_host: self.collaboration.teammate_host.clone(),
            },
            tooling: clone_tooling_services(&self.tooling),
            runtime_intrinsics_enabled: self.runtime_intrinsics_enabled,
            runtime_instance_id: format!("runtime-{}", std::process::id()),
            persisted_runtime_identifier: self.persisted_runtime_identifier.clone(),
            lease_keys: Arc::new(Mutex::new(BTreeSet::new())),
            agent_contexts: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn with_runtime_identifier(&self, runtime_identifier: impl Into<Arc<str>>) -> Self {
        Self {
            execution: self.execution.clone(),
            persistence: PersistenceServices {
                store: self.persistence.store.clone(),
                memory: Arc::new(MemoryEngine::new(
                    self.persistence.store.clone(),
                    self.execution.hooks.clone(),
                )),
            },
            collaboration: CollaborationServices {
                background_tasks: BackgroundTaskManager::new(
                    self.persistence.store.clone(),
                    self.execution.executor.clone(),
                    background_hook_sink(
                        self.persistence.store.clone(),
                        self.execution.hooks.clone(),
                    ),
                ),
                team: self.collaboration.team.clone(),
                teammate_host: self.collaboration.teammate_host.clone(),
            },
            tooling: clone_tooling_services(&self.tooling),
            runtime_intrinsics_enabled: self.runtime_intrinsics_enabled,
            runtime_instance_id: format!("runtime-{}", std::process::id()),
            persisted_runtime_identifier: runtime_identifier.into(),
            lease_keys: Arc::new(Mutex::new(BTreeSet::new())),
            agent_contexts: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}
