use std::path::Path;
use std::sync::Arc;

use crate::{
    provider::{BuiltinProvider, Provider, ProviderRegistry},
    runtime::{
        RuntimeExecutor, RuntimeHandle, RuntimeHook, RuntimeHooks, RuntimePolicy, RuntimeStore,
        error::RuntimeError, skill::SkillLoadError,
    },
    tool::ExecutableTool,
};

use super::Runtime;
use super::skill::SkillLoader;

#[derive(Default)]
pub struct RuntimeBuilder {
    handle: RuntimeHandle,
    provider_registry: ProviderRegistry,
}

impl RuntimeBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn new_empty() -> Self {
        Self {
            handle: RuntimeHandle::new_empty(),
            provider_registry: ProviderRegistry::default(),
        }
    }

    pub fn with_tool<T>(self, tool: T) -> Self
    where
        T: ExecutableTool + 'static,
    {
        self.handle.register_tool(tool);
        self
    }

    pub fn with_intrinsic<T>(self, tool: T) -> Self
    where
        T: ExecutableTool + 'static,
    {
        self.with_tool(tool)
    }

    pub fn with_store(self, store: impl RuntimeStore + 'static) -> Self {
        Self {
            handle: self.handle.rebind_store(std::sync::Arc::new(store)),
            provider_registry: self.provider_registry,
        }
    }

    pub fn with_executor<E>(self, executor: E) -> Self
    where
        E: RuntimeExecutor + 'static,
    {
        Self {
            handle: self.handle.with_executor(Arc::new(executor)),
            provider_registry: self.provider_registry,
        }
    }

    pub fn with_policy(self, policy: RuntimePolicy) -> Self {
        Self {
            handle: self.handle.with_policy(policy),
            provider_registry: self.provider_registry,
        }
    }

    pub fn with_hook<H>(self, hook: H) -> Self
    where
        H: RuntimeHook + 'static,
    {
        Self {
            handle: self.handle.with_hooks(RuntimeHooks::new().with_hook(hook)),
            provider_registry: self.provider_registry,
        }
    }

    pub fn with_hooks<I>(self, hooks: I) -> Self
    where
        I: IntoIterator<Item = Arc<dyn RuntimeHook>>,
    {
        Self {
            handle: self.handle.with_hooks(RuntimeHooks::new().extend(hooks)),
            provider_registry: self.provider_registry,
        }
    }

    pub fn with_skills_dir(self, path: impl AsRef<Path>) -> Result<Self, SkillLoadError> {
        self.handle
            .register_skill_loader(SkillLoader::from_dir(path)?);
        Ok(self)
    }

    pub fn with_optional_provider(
        mut self,
        id: BuiltinProvider,
        api_key: Option<impl Into<String>>,
    ) -> Self {
        if let Some(api_key) = api_key {
            let _ = self
                .provider_registry
                .register_builtin_provider(id, api_key.into());
        }
        self
    }

    pub fn with_provider(mut self, id: BuiltinProvider, api_key: impl Into<String>) -> Self {
        let _ = self
            .provider_registry
            .register_builtin_provider(id, api_key);
        self
    }

    pub fn with_provider_instance<P>(mut self, provider: P) -> Self
    where
        P: Provider + 'static,
    {
        self.provider_registry.register_provider_instance(provider);
        self
    }

    pub fn build(self) -> Result<Runtime, RuntimeError> {
        if self.provider_registry.is_empty() {
            Err(RuntimeError::ProviderNotFound(None))
        } else {
            Ok(Runtime {
                handle: self.handle,
                provider_registry: self.provider_registry,
            })
        }
    }
}
