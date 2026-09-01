use super::*;

impl RuntimeHandle {
    pub fn configure_file_tools(&self, profile: crate::tool::FileToolProfile) {
        self.tooling
            .tool_registry
            .write()
            .expect("tool registry poisoned")
            .configure_file_tools(profile);
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
        self.tooling
            .tool_registry
            .write()
            .expect("tool registry poisoned")
            .register_tool(tool);
    }

    /// Registers a tool unless its name is already taken.
    pub fn try_register_tool<T>(&self, tool: T) -> Result<(), crate::tool::ToolNameCollision>
    where
        T: ExecutableTool + 'static,
    {
        self.tooling
            .tool_registry
            .write()
            .expect("tool registry poisoned")
            .try_register_tool(tool)
    }

    /// Removes a tool by name, reporting whether one was there.
    pub fn unregister_tool_by_name(&self, name: &str) -> bool {
        self.tooling
            .tool_registry
            .write()
            .expect("tool registry poisoned")
            .unregister(name)
    }

    pub(crate) fn register_scoped_tool<T>(
        &self,
        agent_id: &str,
        tool: T,
    ) -> crate::tool::ToolRegistration
    where
        T: ExecutableTool + 'static,
    {
        let mut registry = self
            .tooling
            .tool_registry
            .write()
            .expect("tool registry poisoned");
        let registration = registry.register_tool_tracked(tool);
        let name = registration.descriptor().provider.name.clone();
        self.tooling
            .scoped_tools
            .write()
            .expect("scoped tool registry poisoned")
            .insert(
                name,
                ScopedToolOwner {
                    agent_id: agent_id.to_string(),
                    generation: registration.generation(),
                },
            );
        registration
    }

    pub(crate) fn unregister_scoped_tool(
        &self,
        agent_id: &str,
        registration: &crate::tool::ToolRegistration,
    ) {
        let mut registry = self
            .tooling
            .tool_registry
            .write()
            .expect("tool registry poisoned");
        let mut scoped_tools = self
            .tooling
            .scoped_tools
            .write()
            .expect("scoped tool registry poisoned");
        let owner_matches = scoped_tools.get(registration.name()).is_some_and(|owner| {
            owner.agent_id == agent_id && owner.generation == registration.generation()
        });
        if !owner_matches {
            return;
        }

        registry.unregister_registration(registration);
        scoped_tools.remove(registration.name());
    }

    pub(crate) fn tool_is_visible_to_agent(&self, name: &str, agent_id: &str) -> bool {
        self.tooling
            .scoped_tools
            .read()
            .expect("scoped tool registry poisoned")
            .get(name)
            .is_none_or(|owner| owner.agent_id == agent_id)
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
        self.tooling
            .tool_registry
            .write()
            .expect("tool registry poisoned")
            .register_skill_tool();
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
            self.tooling
                .tool_registry
                .write()
                .expect("tool registry poisoned")
                .unregister_skill_tool();
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

    pub(crate) fn resolve_tool(
        &self,
        name: &str,
    ) -> Option<(Arc<dyn ExecutableTool>, crate::tool::RuntimeToolDescriptor)> {
        let resolved = self
            .tooling
            .tool_registry
            .read()
            .expect("tool registry poisoned")
            .resolve_tool(name)?;
        Some((resolved.handler, resolved.descriptor))
    }
}
