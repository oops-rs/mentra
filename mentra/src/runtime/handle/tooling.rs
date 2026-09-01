use super::*;

impl RuntimeHandle {
    pub fn configure_file_tools(&self, profile: crate::tool::FileToolProfile) {
        let detached_handlers = {
            self.tooling
                .tool_registry
                .write()
                .expect("tool registry poisoned")
                .configure_file_tools(profile)
        };
        drop(detached_handlers);
    }

    pub fn register_app_context(&self, context: Arc<dyn Any + Send + Sync>) {
        self.tooling
            .app_contexts
            .write()
            .expect("app context registry poisoned")
            .insert(context.as_ref().type_id(), context);
    }

    pub fn app_context<T>(&self) -> Result<Arc<T>, String>
    where
        T: Any + Send + Sync + 'static,
    {
        let context = self
            .tooling
            .app_contexts
            .read()
            .expect("app context registry poisoned")
            .get(&TypeId::of::<T>())
            .cloned()
            .ok_or_else(|| {
                format!(
                    "App context '{}' is not registered on this runtime",
                    std::any::type_name::<T>()
                )
            })?;

        Arc::downcast::<T>(context).map_err(|_| {
            format!(
                "App context '{}' was registered with an incompatible type",
                std::any::type_name::<T>()
            )
        })
    }

    pub fn register_tool<T>(&self, tool: T)
    where
        T: ExecutableTool + 'static,
    {
        let prepared = ToolRegistry::prepare_tool(tool);
        let displaced_handlers = {
            let mut registry = self
                .tooling
                .tool_registry
                .write()
                .expect("tool registry poisoned");
            registry.insert_prepared(prepared).into_parts().1
        };
        drop(displaced_handlers);
    }

    /// Registers a tool unless its name is already taken.
    pub fn try_register_tool<T>(&self, tool: T) -> Result<(), crate::tool::ToolNameCollision>
    where
        T: ExecutableTool + 'static,
    {
        let prepared = ToolRegistry::prepare_tool(tool);
        let outcome = {
            let mut registry = self
                .tooling
                .tool_registry
                .write()
                .expect("tool registry poisoned");
            registry.try_insert_prepared(prepared)
        };
        match outcome {
            Ok(insertion) => {
                let (_, displaced_handlers) = insertion.into_parts();
                debug_assert!(displaced_handlers.is_empty());
                Ok(())
            }
            Err((collision, rejected)) => {
                drop(rejected);
                Err(collision)
            }
        }
    }

    pub fn try_register_tool_for_audience<T>(
        &self,
        audience: crate::tool::ToolAudience,
        tool: T,
    ) -> Result<crate::tool::AudienceToolRegistration, crate::tool::ToolNameCollision>
    where
        T: ExecutableTool + 'static,
    {
        let prepared = ToolRegistry::prepare_tool(tool);
        let outcome = {
            let mut registry = self
                .tooling
                .tool_registry
                .write()
                .expect("tool registry poisoned");
            registry.try_insert_audience_prepared(&audience, prepared)
        };
        match outcome {
            Ok(insertion) => {
                let (registration, displaced_handlers) = insertion.into_parts();
                debug_assert!(displaced_handlers.is_empty());
                Ok(crate::tool::AudienceToolRegistration::new(
                    Arc::downgrade(&self.tooling.tool_registry),
                    audience,
                    registration,
                ))
            }
            Err((collision, rejected)) => {
                drop(rejected);
                Err(collision)
            }
        }
    }

    /// Removes a tool by name, reporting whether one was there.
    pub fn unregister_tool_by_name(&self, name: &str) -> bool {
        let detached_handler = {
            self.tooling
                .tool_registry
                .write()
                .expect("tool registry poisoned")
                .detach_tool(name)
        };
        let removed = detached_handler.is_some();
        drop(detached_handler);
        removed
    }

    pub(crate) fn register_agent_tool<T>(
        &self,
        agent_id: &str,
        tool: T,
    ) -> crate::tool::AgentToolRegistration
    where
        T: ExecutableTool + 'static,
    {
        let prepared = ToolRegistry::prepare_tool(tool);
        let (registration, displaced_handlers) = {
            let mut registry = self
                .tooling
                .tool_registry
                .write()
                .expect("tool registry poisoned");
            registry
                .insert_agent_prepared(agent_id, prepared)
                .into_parts()
        };
        drop(displaced_handlers);
        crate::tool::AgentToolRegistration::new(
            Arc::downgrade(&self.tooling.tool_registry),
            agent_id.to_string(),
            registration,
        )
    }

    pub(crate) fn visible_tool_registrations(
        &self,
        agent_id: &str,
    ) -> Vec<crate::tool::ToolRegistration> {
        let registry = self
            .tooling
            .tool_registry
            .read()
            .expect("tool registry poisoned");
        let mut selected = HashMap::new();
        for registration in registry.agent_registrations(agent_id) {
            selected.insert(registration.name().to_string(), registration);
        }
        if let Some(audience) = self.tool_audience() {
            for registration in registry.audience_registrations(audience) {
                selected
                    .entry(registration.name().to_string())
                    .or_insert(registration);
            }
        }
        for registration in registry.registrations() {
            selected
                .entry(registration.name().to_string())
                .or_insert(registration);
        }
        let mut registrations = selected.into_values().collect::<Vec<_>>();
        registrations.sort_by(|left, right| left.name().cmp(right.name()));
        registrations
    }

    pub(crate) fn resolve_tool_for_agent(
        &self,
        name: &str,
        agent_id: &str,
    ) -> crate::tool::ToolResolution {
        let registry = self
            .tooling
            .tool_registry
            .read()
            .expect("tool registry poisoned");
        if let Some(tool) = registry.resolve_agent_tool(agent_id, name) {
            return crate::tool::ToolResolution::Visible(Box::new(tool));
        }
        if let Some(audience) = self.tool_audience()
            && let Some(tool) = registry.resolve_audience_tool(audience, name)
        {
            return crate::tool::ToolResolution::Visible(Box::new(tool));
        }
        if let Some(global) = registry.resolve_tool(name) {
            return crate::tool::ToolResolution::Visible(Box::new(global));
        }
        if registry.any_agent_contains(name) || registry.any_audience_contains(name) {
            crate::tool::ToolResolution::Hidden
        } else {
            crate::tool::ToolResolution::Missing
        }
    }

    /// Commits already-loaded skill roots and enables the `load_skill` tool.
    ///
    /// The roots arrive loaded so that this step cannot fail: a caller that
    /// hit a bad root never gets here, which is what makes a registration call
    /// all-or-nothing. An empty batch is a no-op, tool included.
    pub fn register_skill_roots(&self, roots: Vec<SkillRoot>) {
        if roots.is_empty() {
            return;
        }

        self.skill_registry_mut().insert(roots);
        let displaced_handlers = {
            self.tooling
                .tool_registry
                .write()
                .expect("tool registry poisoned")
                .register_skill_tool()
                .into_parts()
                .1
        };
        drop(displaced_handlers);
    }

    /// Drops every root named by `paths`, reporting whether any was there.
    ///
    /// Removing the last root also withdraws the `load_skill` tool: a tool
    /// that can only answer "Skill loader is not available" is worth tokens to
    /// nobody. It comes back with the next registration.
    pub fn unregister_skill_roots<I, P>(&self, paths: I) -> bool
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut registry = self.skill_registry_mut();
        // Deliberately not `Iterator::any`, which stops at the first match:
        // every path named here must be dropped, not just the first one that
        // happens to be registered.
        let mut removed = false;
        for path in paths {
            removed |= registry.remove(path);
        }
        let empty = registry.is_empty();
        drop(registry);

        if removed && empty {
            let detached_handler = {
                self.tooling
                    .tool_registry
                    .write()
                    .expect("tool registry poisoned")
                    .unregister_skill_tool()
            };
            drop(detached_handler);
        }
        removed
    }

    /// Every loaded skill, name-ordered, without bodies.
    pub fn skills(&self) -> Vec<crate::runtime::SkillInfo> {
        self.skill_registry().infos()
    }

    pub fn tools(&self) -> Arc<[crate::tool::ProviderToolSpec]> {
        self.tooling
            .tool_registry
            .read()
            .expect("tool registry poisoned")
            .tools()
    }

    pub fn store(&self) -> Arc<dyn RuntimeStore> {
        self.persistence.store.clone()
    }

    pub fn persisted_runtime_identifier(&self) -> &str {
        &self.persisted_runtime_identifier
    }

    pub fn skill_descriptions(&self) -> Option<String> {
        let descriptions = self.skill_registry().get_descriptions();
        Some(descriptions).filter(|descriptions| !descriptions.is_empty())
    }

    pub fn load_skill(&self, name: &str) -> Result<String, String> {
        self.skill_registry().get_content(name)
    }

    /// Returns a skill's body whether or not the model may invoke it.
    pub fn skill_body(&self, name: &str) -> Result<String, String> {
        self.skill_registry().get_body(name)
    }

    fn skill_registry(&self) -> RwLockReadGuard<'_, SkillRegistry> {
        self.tooling.skills.read().expect("skill registry poisoned")
    }

    fn skill_registry_mut(&self) -> RwLockWriteGuard<'_, SkillRegistry> {
        self.tooling
            .skills
            .write()
            .expect("skill registry poisoned")
    }

    #[cfg(test)]
    pub fn get_tool(&self, name: &str) -> Option<Arc<dyn ExecutableTool>> {
        self.tooling
            .tool_registry
            .read()
            .expect("tool registry poisoned")
            .get_tool(name)
    }

    pub fn get_tool_descriptor(&self, name: &str) -> Option<crate::tool::RuntimeToolDescriptor> {
        self.tooling
            .tool_registry
            .read()
            .expect("tool registry poisoned")
            .get_tool_descriptor(name)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        panic::{AssertUnwindSafe, catch_unwind},
        sync::mpsc,
        sync::{
            Arc as StdArc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        time::Duration,
    };

    use async_trait::async_trait;

    use super::*;
    use crate::tool::{ToolDefinition, ToolExecutor, ToolSpec};

    struct NamedTool {
        name: &'static str,
        description: &'static str,
    }

    impl ToolDefinition for NamedTool {
        fn descriptor(&self) -> ToolSpec {
            ToolSpec::builder(self.name)
                .description(self.description)
                .build()
        }
    }

    #[async_trait]
    impl ToolExecutor for NamedTool {}

    struct CountingDescriptorTool {
        calls: StdArc<AtomicUsize>,
    }

    impl ToolDefinition for CountingDescriptorTool {
        fn descriptor(&self) -> ToolSpec {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            ToolSpec::builder("counted_audience")
                .description(format!("descriptor call {call}"))
                .build()
        }
    }

    #[async_trait]
    impl ToolExecutor for CountingDescriptorTool {}

    struct ReentrantDropTool {
        runtime: RuntimeHandle,
        name: &'static str,
        probe_name: &'static str,
        dropped: mpsc::Sender<()>,
    }

    impl ToolDefinition for ReentrantDropTool {
        fn descriptor(&self) -> ToolSpec {
            ToolSpec::builder(self.name).build()
        }
    }

    #[async_trait]
    impl ToolExecutor for ReentrantDropTool {}

    fn is_visible(runtime: &RuntimeHandle, name: &str, agent_id: &str) -> bool {
        matches!(
            runtime.resolve_tool_for_agent(name, agent_id),
            crate::tool::ToolResolution::Visible(_)
        )
    }

    fn audience_description(
        runtime: &RuntimeHandle,
        audience: &crate::tool::ToolAudience,
        name: &str,
    ) -> Option<String> {
        runtime
            .tooling
            .tool_registry
            .read()
            .expect("tool registry poisoned")
            .resolve_audience_tool(audience, name)
            .and_then(|tool| tool.descriptor().provider.description.clone())
    }

    impl Drop for ReentrantDropTool {
        fn drop(&mut self) {
            self.runtime.register_tool(NamedTool {
                name: self.probe_name,
                description: "registered from Drop",
            });
            let _ = self.dropped.send(());
        }
    }

    #[test]
    fn global_mutations_evict_agent_entries_without_empowering_old_guards() {
        let runtime = RuntimeHandle::new(false);
        let replaced = runtime.register_agent_tool(
            "owner",
            NamedTool {
                name: "replaced",
                description: "scoped",
            },
        );
        assert!(!is_visible(&runtime, "replaced", "other"));

        runtime.register_tool(NamedTool {
            name: "replaced",
            description: "global",
        });
        assert!(is_visible(&runtime, "replaced", "other"));
        assert!(!replaced.unregister());
        assert_eq!(
            runtime
                .get_tool_descriptor("replaced")
                .expect("global replacement remains")
                .provider
                .description
                .as_deref(),
            Some("global")
        );

        let occupied = runtime.register_agent_tool(
            "owner",
            NamedTool {
                name: "occupied",
                description: "agent",
            },
        );
        assert!(
            runtime
                .try_register_tool(NamedTool {
                    name: "occupied",
                    description: "global",
                })
                .is_err()
        );
        assert!(occupied.unregister());

        runtime.register_tool(NamedTool {
            name: "coexists",
            description: "global",
        });
        let exact = runtime.register_agent_tool(
            "owner",
            NamedTool {
                name: "coexists",
                description: "agent",
            },
        );
        assert!(runtime.unregister_tool_by_name("coexists"));
        assert!(is_visible(&runtime, "coexists", "owner"));
        assert!(!is_visible(&runtime, "coexists", "other"));
        assert!(exact.unregister());
    }

    #[test]
    fn same_name_agent_registrations_coexist_and_drop_independently() {
        let runtime = RuntimeHandle::new(false);
        let first = runtime.register_agent_tool(
            "first",
            NamedTool {
                name: "agent_shared",
                description: "first",
            },
        );
        let second = runtime.register_agent_tool(
            "second",
            NamedTool {
                name: "agent_shared",
                description: "second",
            },
        );
        for (agent_id, expected) in [("first", "first"), ("second", "second")] {
            let crate::tool::ToolResolution::Visible(tool) =
                runtime.resolve_tool_for_agent("agent_shared", agent_id)
            else {
                panic!("{agent_id} resolves its own tool");
            };
            assert_eq!(
                tool.descriptor().provider.description.as_deref(),
                Some(expected)
            );
        }
        assert!(!is_visible(&runtime, "agent_shared", "other"));
        assert!(
            runtime
                .try_register_tool(NamedTool {
                    name: "agent_shared",
                    description: "safe global",
                })
                .is_err()
        );
        assert!(first.unregister());
        assert!(is_visible(&runtime, "agent_shared", "second"));
        assert!(second.unregister());
    }

    #[test]
    fn audience_registration_enforces_scope_collision_matrix_and_global_compatibility() {
        let runtime = RuntimeHandle::new(false);
        let alpha = crate::tool::ToolAudience::new("alpha");
        let beta = crate::tool::ToolAudience::new("beta");
        let alpha_guard = runtime
            .try_register_tool_for_audience(
                alpha.clone(),
                NamedTool {
                    name: "shared_name",
                    description: "alpha",
                },
            )
            .expect("first audience registration");
        let collision = runtime
            .try_register_tool_for_audience(
                alpha.clone(),
                NamedTool {
                    name: "shared_name",
                    description: "duplicate alpha",
                },
            )
            .expect_err("same audience and name collide");
        assert_eq!(collision.name, "shared_name");
        let beta_guard = runtime
            .try_register_tool_for_audience(
                beta.clone(),
                NamedTool {
                    name: "shared_name",
                    description: "beta",
                },
            )
            .expect("different audiences may share a name");
        assert_eq!(
            audience_description(&runtime, &alpha, "shared_name").as_deref(),
            Some("alpha")
        );
        assert_eq!(
            audience_description(&runtime, &beta, "shared_name").as_deref(),
            Some("beta")
        );
        assert!(runtime.get_tool_descriptor("shared_name").is_none());
        assert!(
            runtime
                .tools()
                .iter()
                .all(|tool| tool.name != "shared_name")
        );
        assert!(
            runtime
                .try_register_tool(NamedTool {
                    name: "shared_name",
                    description: "safe global",
                })
                .is_err()
        );

        runtime.register_tool(NamedTool {
            name: "shared_name",
            description: "unsafe global",
        });
        assert_eq!(
            runtime
                .get_tool_descriptor("shared_name")
                .expect("unsafe global registration")
                .provider
                .description
                .as_deref(),
            Some("unsafe global")
        );
        assert!(audience_description(&runtime, &alpha, "shared_name").is_none());
        assert!(audience_description(&runtime, &beta, "shared_name").is_none());
        assert!(
            !alpha_guard.unregister(),
            "global insertion staled alpha guard"
        );
        assert!(
            !beta_guard.unregister(),
            "global insertion staled beta guard"
        );
        assert!(runtime.unregister_tool_by_name("shared_name"));

        runtime.register_tool(NamedTool {
            name: "global_first",
            description: "global",
        });
        assert!(
            runtime
                .try_register_tool_for_audience(
                    alpha,
                    NamedTool {
                        name: "global_first",
                        description: "audience",
                    },
                )
                .is_err()
        );

        let file_audience = crate::tool::ToolAudience::new("file-profile");
        let file_guard = runtime
            .try_register_tool_for_audience(
                file_audience.clone(),
                NamedTool {
                    name: "files",
                    description: "audience files",
                },
            )
            .expect("audience files registration");
        runtime.configure_file_tools(crate::tool::FileToolProfile::Batched);
        assert!(runtime.get_tool_descriptor("files").is_some());
        assert!(audience_description(&runtime, &file_audience, "files").is_none());
        assert!(
            !file_guard.unregister(),
            "builtin insertion staled audience guard"
        );
    }

    #[test]
    fn audience_guard_returns_single_descriptor_snapshot_and_is_aba_safe() {
        let runtime = RuntimeHandle::new(false);
        let audience = crate::tool::ToolAudience::new("counted");
        let calls = StdArc::new(AtomicUsize::new(0));
        let old_guard = runtime
            .try_register_tool_for_audience(
                audience.clone(),
                CountingDescriptorTool {
                    calls: StdArc::clone(&calls),
                },
            )
            .expect("audience registration");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            old_guard.descriptor().provider.description.as_deref(),
            Some("descriptor call 1")
        );

        let detached_handler = runtime
            .tooling
            .tool_registry
            .write()
            .expect("tool registry poisoned")
            .detach_audience_registration(&audience, old_guard.registration());
        drop(detached_handler);
        let new_guard = runtime
            .try_register_tool_for_audience(
                audience.clone(),
                NamedTool {
                    name: "counted_audience",
                    description: "new generation",
                },
            )
            .expect("replacement generation");
        drop(old_guard);
        assert_eq!(
            audience_description(&runtime, &audience, "counted_audience").as_deref(),
            Some("new generation")
        );
        assert!(new_guard.unregister());
        assert!(audience_description(&runtime, &audience, "counted_audience").is_none());
    }

    #[test]
    fn replacing_a_tool_drops_its_handler_after_registry_unlock() {
        let runtime = RuntimeHandle::new(false);
        let (dropped, observed) = mpsc::channel();
        runtime.register_tool(ReentrantDropTool {
            runtime: runtime.clone(),
            name: "reentrant_replace",
            probe_name: "replace_drop_probe",
            dropped,
        });

        let worker_runtime = runtime.clone();
        let worker = thread::spawn(move || {
            worker_runtime.register_tool(NamedTool {
                name: "reentrant_replace",
                description: "replacement",
            });
        });
        observed
            .recv_timeout(Duration::from_secs(5))
            .expect("reentrant Drop must not deadlock on the registry lock");
        worker.join().expect("replacement worker");
        assert!(runtime.get_tool_descriptor("replace_drop_probe").is_some());
    }

    #[test]
    fn agent_guard_drops_its_handler_after_registry_unlock() {
        let runtime = RuntimeHandle::new(false);
        let (dropped, observed) = mpsc::channel();
        let registration = runtime.register_agent_tool(
            "owner",
            ReentrantDropTool {
                runtime: runtime.clone(),
                name: "reentrant_agent",
                probe_name: "agent_drop_probe",
                dropped,
            },
        );

        let worker = thread::spawn(move || drop(registration));
        observed
            .recv_timeout(Duration::from_secs(5))
            .expect("reentrant Drop must not deadlock on agent registry lock");
        worker.join().expect("unregister worker");
        assert!(runtime.get_tool_descriptor("agent_drop_probe").is_some());
    }

    #[test]
    fn agent_guard_recovers_a_poisoned_registry_without_panicking() {
        let runtime = RuntimeHandle::new(false);
        let guard = runtime.register_agent_tool(
            "owner",
            NamedTool {
                name: "poisoned_agent_tool",
                description: "registered",
            },
        );
        let registry = Arc::clone(&runtime.tooling.tool_registry);
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _lock = registry.write().expect("unpoisoned before test");
                panic!("poison registry");
            }))
            .is_err()
        );
        assert!(catch_unwind(AssertUnwindSafe(|| drop(guard))).is_ok());
        assert!(
            registry
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .resolve_agent_tool("owner", "poisoned_agent_tool")
                .is_none()
        );
    }

    #[test]
    fn audience_guard_drops_its_handler_after_registry_unlock() {
        let runtime = RuntimeHandle::new(false);
        let audience = crate::tool::ToolAudience::new("drop");
        let (dropped, observed) = mpsc::channel();
        let guard = runtime
            .try_register_tool_for_audience(
                audience,
                ReentrantDropTool {
                    runtime: runtime.clone(),
                    name: "reentrant_audience",
                    probe_name: "audience_drop_probe",
                    dropped,
                },
            )
            .expect("audience registration");

        let worker = thread::spawn(move || drop(guard));
        observed
            .recv_timeout(Duration::from_secs(5))
            .expect("reentrant Drop must not deadlock on audience registry lock");
        worker.join().expect("guard drop worker");
        assert!(runtime.get_tool_descriptor("audience_drop_probe").is_some());
    }

    #[test]
    fn audience_guard_recovers_a_poisoned_registry_without_panicking() {
        let runtime = RuntimeHandle::new(false);
        let audience = crate::tool::ToolAudience::new("poisoned");
        let guard = runtime
            .try_register_tool_for_audience(
                audience.clone(),
                NamedTool {
                    name: "poisoned_tool",
                    description: "registered",
                },
            )
            .expect("audience registration");
        let registry = Arc::clone(&runtime.tooling.tool_registry);
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _lock = registry.write().expect("unpoisoned before test");
                panic!("poison registry");
            }))
            .is_err()
        );

        assert!(catch_unwind(AssertUnwindSafe(|| drop(guard))).is_ok());
        assert!(
            registry
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .resolve_audience_tool(&audience, "poisoned_tool")
                .is_none()
        );
    }

    #[test]
    fn rejected_audience_handler_is_dropped_after_registry_unlock() {
        let runtime = RuntimeHandle::new(false);
        let audience = crate::tool::ToolAudience::new("collision-drop");
        let _guard = runtime
            .try_register_tool_for_audience(
                audience.clone(),
                NamedTool {
                    name: "audience_collision",
                    description: "first",
                },
            )
            .expect("first registration");
        let (dropped, observed) = mpsc::channel();
        let worker_runtime = runtime.clone();
        let worker = thread::spawn(move || {
            worker_runtime
                .try_register_tool_for_audience(
                    audience,
                    ReentrantDropTool {
                        runtime: worker_runtime.clone(),
                        name: "audience_collision",
                        probe_name: "audience_rejection_probe",
                        dropped,
                    },
                )
                .expect_err("same audience collision");
        });
        observed
            .recv_timeout(Duration::from_secs(5))
            .expect("rejected handler Drop must not hold registry lock");
        worker.join().expect("collision worker");
        assert!(
            runtime
                .get_tool_descriptor("audience_rejection_probe")
                .is_some()
        );
    }

    #[test]
    fn global_eviction_drops_audience_handlers_after_registry_unlock() {
        let runtime = RuntimeHandle::new(false);
        let (dropped, observed) = mpsc::channel();
        let guard = runtime
            .try_register_tool_for_audience(
                crate::tool::ToolAudience::new("evicted"),
                ReentrantDropTool {
                    runtime: runtime.clone(),
                    name: "audience_evict",
                    probe_name: "audience_eviction_probe",
                    dropped,
                },
            )
            .expect("audience registration");
        let worker_runtime = runtime.clone();
        let worker = thread::spawn(move || {
            worker_runtime.register_tool(NamedTool {
                name: "audience_evict",
                description: "global replacement",
            });
        });
        observed
            .recv_timeout(Duration::from_secs(5))
            .expect("evicted audience handler Drop must not hold registry locks");
        worker.join().expect("global replacement worker");
        assert!(!guard.unregister());
        assert!(
            runtime
                .get_tool_descriptor("audience_eviction_probe")
                .is_some()
        );
    }
}
