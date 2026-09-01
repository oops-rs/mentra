use super::*;

impl RuntimeHandle {
    pub fn configure_file_tools(&self, profile: crate::tool::FileToolProfile) {
        let detached_handlers = {
            let (mut registry, mut scoped_tools) = self.tool_registries_mut();
            let detached_handlers = registry.configure_file_tools(profile);
            for name in ["files", "read", "ls", "grep", "glob", "write", "edit"] {
                scoped_tools.remove(name);
            }
            detached_handlers
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
        let name = prepared.name().to_string();
        let displaced_handlers = {
            let (mut registry, mut scoped_tools) = self.tool_registries_mut();
            let (_, displaced_handlers) = registry.insert_prepared(prepared).into_parts();
            scoped_tools.remove(&name);
            displaced_handlers
        };
        drop(displaced_handlers);
    }

    /// Registers a tool unless its name is already taken.
    pub fn try_register_tool<T>(&self, tool: T) -> Result<(), crate::tool::ToolNameCollision>
    where
        T: ExecutableTool + 'static,
    {
        let prepared = ToolRegistry::prepare_tool(tool);
        let name = prepared.name().to_string();
        let outcome = {
            let (mut registry, mut scoped_tools) = self.tool_registries_mut();
            match registry.try_insert_prepared(prepared) {
                Ok(insertion) => {
                    scoped_tools.remove(&name);
                    Ok(insertion.into_parts().1)
                }
                Err(error) => Err(error),
            }
        };
        match outcome {
            Ok(displaced_handlers) => {
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
            let (mut registry, mut scoped_tools) = self.tool_registries_mut();
            let detached_handler = registry.detach_tool(name);
            scoped_tools.remove(name);
            detached_handler
        };
        let removed = detached_handler.is_some();
        drop(detached_handler);
        removed
    }

    pub(crate) fn register_scoped_tool<T>(
        &self,
        agent_id: &str,
        tool: T,
    ) -> crate::tool::ToolRegistration
    where
        T: ExecutableTool + 'static,
    {
        let prepared = ToolRegistry::prepare_tool(tool);
        let name = prepared.name().to_string();
        let (registration, displaced_handlers) = {
            let (mut registry, mut scoped_tools) = self.tool_registries_mut();
            let (registration, displaced_handlers) =
                registry.insert_prepared(prepared).into_parts();
            scoped_tools.insert(
                name,
                ScopedToolOwner {
                    agent_id: agent_id.to_string(),
                    generation: registration.generation(),
                },
            );
            (registration, displaced_handlers)
        };
        drop(displaced_handlers);
        registration
    }

    pub(crate) fn unregister_scoped_tool(
        &self,
        agent_id: &str,
        registration: &crate::tool::ToolRegistration,
    ) {
        let detached_handler = {
            let (mut registry, mut scoped_tools) = self.tool_registries_mut();
            let owner_matches = scoped_tools.get(registration.name()).is_some_and(|owner| {
                owner.agent_id == agent_id && owner.generation == registration.generation()
            });
            if !owner_matches {
                None
            } else {
                let detached_handler = registry.detach_registration(registration);
                scoped_tools.remove(registration.name());
                detached_handler
            }
        };
        drop(detached_handler);
    }

    pub(crate) fn visible_tool_registrations(
        &self,
        agent_id: &str,
    ) -> Vec<crate::tool::ToolRegistration> {
        let (registry, scoped_tools) = self.tool_registries();
        let global = registry.registrations();
        let mut selected = HashMap::new();

        for registration in &global {
            if scoped_tools.get(registration.name()).is_some_and(|owner| {
                owner.generation == registration.generation() && owner.agent_id == agent_id
            }) {
                selected.insert(registration.name().to_string(), registration.clone());
            }
        }
        if let Some(audience) = self.tool_audience() {
            for registration in registry.audience_registrations(audience) {
                selected
                    .entry(registration.name().to_string())
                    .or_insert(registration);
            }
        }
        for registration in global {
            let actively_scoped = scoped_tools
                .get(registration.name())
                .is_some_and(|owner| owner.generation == registration.generation());
            if !actively_scoped {
                selected
                    .entry(registration.name().to_string())
                    .or_insert(registration);
            }
        }
        selected.into_values().collect()
    }

    pub(crate) fn resolve_tool_for_agent(
        &self,
        name: &str,
        agent_id: &str,
    ) -> crate::tool::ToolResolution {
        let (registry, scoped_tools) = self.tool_registries();
        let global = registry.resolve_tool(name);
        let mut foreign_exact = false;
        if let Some(global) = global.as_ref()
            && let Some(owner) = scoped_tools
                .get(name)
                .filter(|owner| owner.generation == global.registration.generation())
        {
            if owner.agent_id == agent_id {
                return crate::tool::ToolResolution::Visible(Box::new(global.clone()));
            }
            foreign_exact = true;
        }
        if let Some(audience) = self.tool_audience()
            && let Some(tool) = registry.resolve_audience_tool(audience, name)
        {
            return crate::tool::ToolResolution::Visible(Box::new(tool));
        }
        if let Some(global) = global
            && !foreign_exact
        {
            return crate::tool::ToolResolution::Visible(Box::new(global));
        }
        if foreign_exact || registry.any_audience_contains(name) {
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
            let (mut registry, mut scoped_tools) = self.tool_registries_mut();
            let (registration, displaced_handlers) = registry.register_skill_tool().into_parts();
            scoped_tools.remove(registration.name());
            displaced_handlers
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
                let (mut registry, mut scoped_tools) = self.tool_registries_mut();
                let detached_handler = registry.unregister_skill_tool();
                scoped_tools.remove(crate::tool::LOAD_SKILL_TOOL_NAME);
                detached_handler
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

    fn tool_registries_mut(
        &self,
    ) -> (
        RwLockWriteGuard<'_, ToolRegistry>,
        RwLockWriteGuard<'_, HashMap<String, ScopedToolOwner>>,
    ) {
        let registry = self
            .tooling
            .tool_registry
            .write()
            .expect("tool registry poisoned");
        let scoped_tools = self
            .tooling
            .scoped_tools
            .write()
            .expect("scoped tool registry poisoned");
        (registry, scoped_tools)
    }

    fn tool_registries(
        &self,
    ) -> (
        RwLockReadGuard<'_, ToolRegistry>,
        RwLockReadGuard<'_, HashMap<String, ScopedToolOwner>>,
    ) {
        let registry = self
            .tooling
            .tool_registry
            .read()
            .expect("tool registry poisoned");
        let scoped_tools = self
            .tooling
            .scoped_tools
            .read()
            .expect("scoped tool registry poisoned");
        (registry, scoped_tools)
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
        time::{Duration, Instant},
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
    fn global_mutations_clear_scoped_ownership_without_empowering_old_guards() {
        let runtime = RuntimeHandle::new(false);
        let replaced = runtime.register_scoped_tool(
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
        runtime.unregister_scoped_tool("owner", &replaced);
        assert_eq!(
            runtime
                .get_tool_descriptor("replaced")
                .expect("global replacement remains")
                .provider
                .description
                .as_deref(),
            Some("global")
        );

        let stale = runtime.register_scoped_tool(
            "owner",
            NamedTool {
                name: "stale",
                description: "scoped",
            },
        );
        let detached_handler = runtime
            .tooling
            .tool_registry
            .write()
            .expect("tool registry poisoned")
            .detach_registration(&stale);
        drop(detached_handler);
        runtime
            .try_register_tool(NamedTool {
                name: "stale",
                description: "global",
            })
            .expect("stale ownership does not block a free global name");
        assert!(is_visible(&runtime, "stale", "other"));
        assert_eq!(
            runtime
                .visible_tool_registrations("other")
                .into_iter()
                .find(|registration| registration.name() == "stale")
                .expect("stale marker is ignored by the roster")
                .descriptor()
                .provider
                .description
                .as_deref(),
            Some("global")
        );
        runtime.unregister_scoped_tool("owner", &stale);
        assert!(runtime.get_tool_descriptor("stale").is_some());

        let removed = runtime.register_scoped_tool(
            "owner",
            NamedTool {
                name: "removed",
                description: "scoped",
            },
        );
        assert!(runtime.unregister_tool_by_name("removed"));
        runtime.register_tool(NamedTool {
            name: "removed",
            description: "global",
        });
        runtime.unregister_scoped_tool("owner", &removed);
        assert!(is_visible(&runtime, "removed", "other"));
        assert!(runtime.get_tool_descriptor("removed").is_some());
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
    fn roster_reader_cannot_split_a_global_to_scoped_registration() {
        let runtime = RuntimeHandle::new(false);
        runtime.register_tool(NamedTool {
            name: "interleaved",
            description: "global",
        });
        let scoped_lock = runtime
            .tooling
            .scoped_tools
            .write()
            .expect("scoped tool registry poisoned");
        let writer_runtime = runtime.clone();
        let (registered, registration_rx) = mpsc::channel();
        let writer = thread::spawn(move || {
            let registration = writer_runtime.register_scoped_tool(
                "owner",
                NamedTool {
                    name: "interleaved",
                    description: "scoped",
                },
            );
            registered.send(registration).expect("registration result");
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while runtime.tooling.tool_registry.try_read().is_ok() {
            assert!(
                Instant::now() < deadline,
                "writer never acquired registry lock"
            );
            thread::yield_now();
        }
        let reader_runtime = runtime.clone();
        let reader = thread::spawn(move || reader_runtime.visible_tool_registrations("other"));
        drop(scoped_lock);

        let registration = registration_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("scoped registration completes");
        let foreign_roster = reader.join().expect("roster reader");
        writer.join().expect("registration writer");
        assert!(
            foreign_roster
                .iter()
                .all(|tool| tool.name() != "interleaved"),
            "foreign reader sees neither stale global spec nor scoped replacement"
        );
        let owner = runtime
            .visible_tool_registrations("owner")
            .into_iter()
            .find(|tool| tool.name() == "interleaved")
            .expect("owner sees scoped registration");
        assert_eq!(
            owner.descriptor().provider.description.as_deref(),
            Some("scoped")
        );
        runtime.unregister_scoped_tool("owner", &registration);
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
    fn scoped_unregister_drops_its_handler_after_registry_unlock() {
        let runtime = RuntimeHandle::new(false);
        let (dropped, observed) = mpsc::channel();
        let registration = runtime.register_scoped_tool(
            "owner",
            ReentrantDropTool {
                runtime: runtime.clone(),
                name: "reentrant_scoped",
                probe_name: "scoped_drop_probe",
                dropped,
            },
        );

        let worker_runtime = runtime.clone();
        let worker = thread::spawn(move || {
            worker_runtime.unregister_scoped_tool("owner", &registration);
        });
        observed
            .recv_timeout(Duration::from_secs(5))
            .expect("reentrant Drop must not deadlock on scoped registry locks");
        worker.join().expect("unregister worker");
        assert!(runtime.get_tool_descriptor("scoped_drop_probe").is_some());
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
