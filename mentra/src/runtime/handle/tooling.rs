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
