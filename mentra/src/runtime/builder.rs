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

/// Builder for constructing a [`Runtime`] with providers, tools, and policies.
#[derive(Default)]
pub struct RuntimeBuilder {
    handle: RuntimeHandle,
    provider_registry: ProviderRegistry,
}

impl RuntimeBuilder {
    /// Creates a builder with Mentra's builtin tools enabled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a builder without builtin tools.
    pub fn new_empty() -> Self {
        Self {
            handle: RuntimeHandle::new_empty(),
            provider_registry: ProviderRegistry::default(),
        }
    }

    /// Registers a custom tool.
    pub fn with_tool<T>(self, tool: T) -> Self
    where
        T: ExecutableTool + 'static,
    {
        self.handle.register_tool(tool);
        self
    }

    /// Registers a runtime intrinsic tool.
    pub fn with_intrinsic<T>(self, tool: T) -> Self
    where
        T: ExecutableTool + 'static,
    {
        self.with_tool(tool)
    }

    /// Replaces the runtime store implementation.
    pub fn with_store(self, store: impl RuntimeStore + 'static) -> Self {
        Self {
            handle: self.handle.rebind_store(std::sync::Arc::new(store)),
            provider_registry: self.provider_registry,
        }
    }

    /// Replaces the command executor used by builtin tools.
    pub fn with_executor<E>(self, executor: E) -> Self
    where
        E: RuntimeExecutor + 'static,
    {
        Self {
            handle: self.handle.with_executor(Arc::new(executor)),
            provider_registry: self.provider_registry,
        }
    }

    /// Sets the runtime policy used to authorize file and process access.
    pub fn with_policy(self, policy: RuntimePolicy) -> Self {
        Self {
            handle: self.handle.with_policy(policy),
            provider_registry: self.provider_registry,
        }
    }

    /// Appends a single runtime hook.
    pub fn with_hook<H>(self, hook: H) -> Self
    where
        H: RuntimeHook + 'static,
    {
        Self {
            handle: self.handle.with_hooks(RuntimeHooks::new().with_hook(hook)),
            provider_registry: self.provider_registry,
        }
    }

    /// Replaces hooks with the provided collection.
    pub fn with_hooks<I>(self, hooks: I) -> Self
    where
        I: IntoIterator<Item = Arc<dyn RuntimeHook>>,
    {
        Self {
            handle: self.handle.with_hooks(RuntimeHooks::new().extend(hooks)),
            provider_registry: self.provider_registry,
        }
    }

    /// Registers a skills directory and enables the builtin `load_skill` tool.
    pub fn with_skills_dir(self, path: impl AsRef<Path>) -> Result<Self, SkillLoadError> {
        self.handle
            .register_skill_loader(SkillLoader::from_dir(path)?);
        Ok(self)
    }

    /// Registers a builtin provider when an API key is present.
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

    /// Registers a builtin provider from an API key.
    pub fn with_provider(mut self, id: BuiltinProvider, api_key: impl Into<String>) -> Self {
        let _ = self
            .provider_registry
            .register_builtin_provider(id, api_key);
        self
    }

    /// Registers a custom provider implementation.
    pub fn with_provider_instance<P>(mut self, provider: P) -> Self
    where
        P: Provider + 'static,
    {
        self.provider_registry.register_provider_instance(provider);
        self
    }

    /// Builds the runtime and validates that at least one provider is registered.
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
