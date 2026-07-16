use super::*;

impl RuntimeHandle {
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

    pub(crate) fn register_scoped_tool<T>(&self, agent_id: &str, tool: T)
    where
        T: ExecutableTool + 'static,
    {
        let name = tool.descriptor().provider.name;
        self.tooling
            .scoped_tools
            .write()
            .expect("scoped tool registry poisoned")
            .insert(name, agent_id.to_string());
        self.register_tool(tool);
    }

    pub(crate) fn unregister_scoped_tool(&self, agent_id: &str, name: &str) {
        let owner_matches = self
            .tooling
            .scoped_tools
            .read()
            .expect("scoped tool registry poisoned")
            .get(name)
            .is_some_and(|owner| owner == agent_id);
        if !owner_matches {
            return;
        }

        self.tooling
            .tool_registry
            .write()
            .expect("tool registry poisoned")
            .unregister_tool(name);
        self.tooling
            .scoped_tools
            .write()
            .expect("scoped tool registry poisoned")
            .remove(name);
    }

    pub(crate) fn tool_is_visible_to_agent(&self, name: &str, agent_id: &str) -> bool {
        self.tooling
            .scoped_tools
            .read()
            .expect("scoped tool registry poisoned")
            .get(name)
            .is_none_or(|owner| owner == agent_id)
    }

    pub fn register_skill_loader(&self, loader: SkillLoader) {
        *self
            .tooling
            .skill_loader
            .write()
            .expect("skill loader poisoned") = Some(loader);
        self.tooling
            .tool_registry
            .write()
            .expect("tool registry poisoned")
            .register_skill_tool();
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
        self.tooling
            .skill_loader
            .read()
            .expect("skill loader poisoned")
            .as_ref()
            .map(SkillLoader::get_descriptions)
            .filter(|descriptions| !descriptions.is_empty())
    }

    pub fn load_skill(&self, name: &str) -> Result<String, String> {
        let skills = self
            .tooling
            .skill_loader
            .read()
            .expect("skill loader poisoned");
        let Some(loader) = skills.as_ref() else {
            return Err("Skill loader is not available".to_string());
        };

        loader.get_content(name)
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
}
