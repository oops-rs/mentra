use super::*;
use crate::background::BackgroundHookSink;
use crate::compaction::StandardCompactionEngine;
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
        self.hooks.emit_runtime(
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
        self.hooks.emit_runtime(
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
    let (tool_registry, scoped_tools) = {
        let tool_registry = tooling
            .tool_registry
            .read()
            .expect("tool registry poisoned");
        let scoped_tools = tooling
            .scoped_tools
            .read()
            .expect("scoped tool registry poisoned");
        (tool_registry.clone(), scoped_tools.clone())
    };
    ToolingServices {
        tool_registry: Arc::new(RwLock::new(tool_registry)),
        scoped_tools: Arc::new(RwLock::new(scoped_tools)),
        skills: Arc::new(RwLock::new(
            tooling
                .skills
                .read()
                .expect("skill registry poisoned")
                .clone(),
        )),
        app_contexts: tooling.app_contexts.clone(),
    }
}

impl RuntimeHandle {
    /// Assembles a handle around the default store, without opening it.
    ///
    /// The default persistent store is SQLite-backed when the `store-sqlite`
    /// feature is on (the default), and the file-backed store under the same
    /// default path policy when it is off.
    ///
    /// A builder may replace the store before it settles, so nothing here may
    /// touch the disk: constructing either default only records a path, and
    /// the first write (or recovery) is what creates anything. Recovery is
    /// deferred to [`prepare_recovery`](Self::prepare_recovery), which the
    /// builder calls once on whichever store the caller kept.
    pub fn new(runtime_intrinsics_enabled: bool) -> Self {
        #[cfg(feature = "store-sqlite")]
        let store: Arc<dyn RuntimeStore> =
            Arc::new(crate::runtime::sqlite_store::SqliteRuntimeStore::default());
        #[cfg(not(feature = "store-sqlite"))]
        let store: Arc<dyn RuntimeStore> = Arc::new(crate::runtime::FileRuntimeStore::default());
        let executor: Arc<dyn RuntimeExecutor> = Arc::new(LocalRuntimeExecutor);
        let policy = Arc::new(RuntimePolicy::default());
        let hooks = RuntimeHooks::new().with_hook(AuditHook);
        let compaction: Arc<dyn crate::compaction::CompactionEngine> =
            Arc::new(StandardCompactionEngine);
        let runtime_instance_id = format!("runtime-{}", std::process::id());
        let memory = Arc::new(MemoryEngine::new(store.clone(), hooks.clone()));
        let mut tool_registry = ToolRegistry::default();
        if runtime_intrinsics_enabled {
            crate::runtime::intrinsic::register_tools(&mut tool_registry);
            tool_registry.register_builtin_tools(crate::tool::FileToolProfile::default());
        }
        Self {
            execution: ExecutionServices {
                executor: executor.clone(),
                policy,
                tool_authorizer: None,
                hooks: hooks.clone(),
                pre_hooks: PreExecutionHooks::new(),
                post_hooks: PostExecutionHooks::new(),
            },
            persistence: PersistenceServices {
                store: store.clone(),
                memory,
                compaction,
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
                scoped_tools: Arc::new(RwLock::new(HashMap::new())),
                skills: Arc::new(RwLock::new(SkillRegistry::default())),
                app_contexts: Arc::new(RwLock::new(HashMap::new())),
            },
            runtime_intrinsics_enabled,
            runtime_instance_id,
            persisted_runtime_identifier: Arc::<str>::from("default"),
            lease_keys: Arc::new(Mutex::new(BTreeSet::new())),
            agent_contexts: Arc::new(RwLock::new(HashMap::new())),
            provider_registry: Arc::new(RwLock::new(ProviderRegistry::default())),
        }
    }

    /// Reconciles interrupted state on this handle's store and announces it.
    ///
    /// The builder calls this once, at the build boundary, because that is the
    /// first moment the store is known to be final. Calling it earlier — from
    /// [`new`](Self::new) or from [`rebind_store`](Self::rebind_store) — opens
    /// a database the caller may be about to discard, and writes a second
    /// `RecoveryPrepared` audit row that makes "how many times did this runtime
    /// start?" unanswerable from the audit trail.
    ///
    /// Recovery is best-effort: a store that cannot reconcile its interrupted
    /// state does not sink an otherwise usable runtime.
    pub fn prepare_recovery(&self) {
        let _ = self.persistence.store.prepare_recovery();
        let _ = self.emit_hook(RuntimeHookEvent::RecoveryPrepared {
            runtime_instance_id: self.runtime_instance_id.clone(),
        });
    }

    /// Returns a handle backed by `store` instead of this one's.
    ///
    /// The replacement is not prepared here; see
    /// [`prepare_recovery`](Self::prepare_recovery) for why that waits for the
    /// build boundary.
    pub fn rebind_store(&self, store: Arc<dyn RuntimeStore>) -> Self {
        Self {
            execution: self.execution.clone(),
            persistence: PersistenceServices {
                store: store.clone(),
                memory: Arc::new(MemoryEngine::new(
                    store.clone(),
                    self.execution.hooks.clone(),
                )),
                compaction: self.persistence.compaction.clone(),
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
            provider_registry: self.provider_registry.clone(),
        }
    }

    pub fn with_executor(&self, executor: Arc<dyn RuntimeExecutor>) -> Self {
        Self {
            execution: ExecutionServices {
                executor: executor.clone(),
                policy: self.execution.policy.clone(),
                tool_authorizer: self.execution.tool_authorizer.clone(),
                hooks: self.execution.hooks.clone(),
                pre_hooks: self.execution.pre_hooks.clone(),
                post_hooks: self.execution.post_hooks.clone(),
            },
            persistence: PersistenceServices {
                store: self.persistence.store.clone(),
                memory: Arc::new(MemoryEngine::new(
                    self.persistence.store.clone(),
                    self.execution.hooks.clone(),
                )),
                compaction: self.persistence.compaction.clone(),
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
            provider_registry: self.provider_registry.clone(),
        }
    }

    pub fn with_policy(&self, policy: RuntimePolicy) -> Self {
        Self {
            execution: ExecutionServices {
                executor: self.execution.executor.clone(),
                policy: Arc::new(policy),
                tool_authorizer: self.execution.tool_authorizer.clone(),
                hooks: self.execution.hooks.clone(),
                pre_hooks: self.execution.pre_hooks.clone(),
                post_hooks: self.execution.post_hooks.clone(),
            },
            persistence: PersistenceServices {
                store: self.persistence.store.clone(),
                memory: Arc::new(MemoryEngine::new(
                    self.persistence.store.clone(),
                    self.execution.hooks.clone(),
                )),
                compaction: self.persistence.compaction.clone(),
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
            provider_registry: self.provider_registry.clone(),
        }
    }

    pub fn with_hooks(&self, hooks: RuntimeHooks) -> Self {
        Self {
            execution: ExecutionServices {
                executor: self.execution.executor.clone(),
                policy: self.execution.policy.clone(),
                tool_authorizer: self.execution.tool_authorizer.clone(),
                hooks: hooks.clone(),
                pre_hooks: self.execution.pre_hooks.clone(),
                post_hooks: self.execution.post_hooks.clone(),
            },
            persistence: PersistenceServices {
                store: self.persistence.store.clone(),
                memory: Arc::new(MemoryEngine::new(
                    self.persistence.store.clone(),
                    hooks.clone(),
                )),
                compaction: self.persistence.compaction.clone(),
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
            provider_registry: self.provider_registry.clone(),
        }
    }

    pub fn with_post_hooks(&self, post_hooks: PostExecutionHooks) -> Self {
        Self {
            execution: ExecutionServices {
                executor: self.execution.executor.clone(),
                policy: self.execution.policy.clone(),
                tool_authorizer: self.execution.tool_authorizer.clone(),
                hooks: self.execution.hooks.clone(),
                pre_hooks: self.execution.pre_hooks.clone(),
                post_hooks,
            },
            persistence: PersistenceServices {
                store: self.persistence.store.clone(),
                memory: Arc::new(MemoryEngine::new(
                    self.persistence.store.clone(),
                    self.execution.hooks.clone(),
                )),
                compaction: self.persistence.compaction.clone(),
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
            provider_registry: self.provider_registry.clone(),
        }
    }

    pub fn with_pre_hooks(&self, pre_hooks: PreExecutionHooks) -> Self {
        Self {
            execution: ExecutionServices {
                executor: self.execution.executor.clone(),
                policy: self.execution.policy.clone(),
                tool_authorizer: self.execution.tool_authorizer.clone(),
                hooks: self.execution.hooks.clone(),
                pre_hooks,
                post_hooks: self.execution.post_hooks.clone(),
            },
            persistence: PersistenceServices {
                store: self.persistence.store.clone(),
                memory: Arc::new(MemoryEngine::new(
                    self.persistence.store.clone(),
                    self.execution.hooks.clone(),
                )),
                compaction: self.persistence.compaction.clone(),
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
            provider_registry: self.provider_registry.clone(),
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
                compaction: self.persistence.compaction.clone(),
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
            provider_registry: self.provider_registry.clone(),
        }
    }

    pub fn with_tool_authorizer(&self, tool_authorizer: Arc<dyn ToolAuthorizer>) -> Self {
        Self {
            execution: ExecutionServices {
                executor: self.execution.executor.clone(),
                policy: self.execution.policy.clone(),
                tool_authorizer: Some(tool_authorizer),
                hooks: self.execution.hooks.clone(),
                pre_hooks: self.execution.pre_hooks.clone(),
                post_hooks: self.execution.post_hooks.clone(),
            },
            persistence: PersistenceServices {
                store: self.persistence.store.clone(),
                memory: Arc::new(MemoryEngine::new(
                    self.persistence.store.clone(),
                    self.execution.hooks.clone(),
                )),
                compaction: self.persistence.compaction.clone(),
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
            provider_registry: self.provider_registry.clone(),
        }
    }

    pub fn with_compaction_engine(
        &self,
        compaction: Arc<dyn crate::compaction::CompactionEngine>,
    ) -> Self {
        Self {
            execution: self.execution.clone(),
            persistence: PersistenceServices {
                store: self.persistence.store.clone(),
                memory: Arc::new(MemoryEngine::new(
                    self.persistence.store.clone(),
                    self.execution.hooks.clone(),
                )),
                compaction,
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
            provider_registry: self.provider_registry.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    use async_trait::async_trait;

    use super::*;
    use crate::tool::{ToolDefinition, ToolExecutor, ToolResolution, ToolSpec};

    struct NamedTool {
        description: &'static str,
    }

    impl ToolDefinition for NamedTool {
        fn descriptor(&self) -> ToolSpec {
            ToolSpec::builder("clone_race")
                .description(self.description)
                .build()
        }
    }

    #[async_trait]
    impl ToolExecutor for NamedTool {}

    #[test]
    fn tooling_clone_cannot_pair_a_scoped_generation_with_a_cleared_marker() {
        let runtime = RuntimeHandle::new(false);
        let scoped = runtime.register_scoped_tool(
            "owner",
            NamedTool {
                description: "scoped",
            },
        );
        let scoped_lock = runtime
            .tooling
            .scoped_tools
            .write()
            .expect("scoped tool registry poisoned");
        let tooling = runtime.tooling.clone();
        let cloner = thread::spawn(move || clone_tooling_services(&tooling));

        let deadline = Instant::now() + Duration::from_secs(5);
        while runtime.tooling.tool_registry.try_write().is_ok() {
            assert!(
                Instant::now() < deadline,
                "cloner never acquired registry lock"
            );
            thread::yield_now();
        }
        let writer_runtime = runtime.clone();
        let (started, writer_started) = mpsc::channel();
        let writer = thread::spawn(move || {
            started.send(()).expect("announce writer");
            writer_runtime.register_tool(NamedTool {
                description: "global",
            });
        });
        writer_started.recv().expect("writer started");
        drop(scoped_lock);

        let cloned_tooling = cloner.join().expect("tooling clone");
        writer.join().expect("global replacement");
        let mut cloned_runtime = runtime.clone();
        cloned_runtime.tooling = cloned_tooling;

        assert!(matches!(
            cloned_runtime.resolve_tool_for_agent("clone_race", "other"),
            ToolResolution::Hidden
        ));
        let ToolResolution::Visible(owner_tool) =
            cloned_runtime.resolve_tool_for_agent("clone_race", "owner")
        else {
            panic!("owner must see coherent scoped clone");
        };
        assert_eq!(
            owner_tool.descriptor().provider.description.as_deref(),
            Some("scoped")
        );
        let ToolResolution::Visible(global_tool) =
            runtime.resolve_tool_for_agent("clone_race", "other")
        else {
            panic!("source runtime must contain global replacement");
        };
        assert_eq!(
            global_tool.descriptor().provider.description.as_deref(),
            Some("global")
        );
        runtime.unregister_scoped_tool("owner", &scoped);
    }
}
