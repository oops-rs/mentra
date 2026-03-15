use std::{any::Any, path::Path, sync::Arc};

use crate::{
    provider::{BuiltinProvider, Provider, ProviderRegistry},
    runtime::{
        RuntimeExecutor, RuntimeHandle, RuntimeHook, RuntimeHooks, RuntimePolicy, RuntimeStore,
        error::RuntimeError, skill::SkillLoadError,
    },
    tool::{ExecutableTool, ToolAuthorizer},
};

use super::Runtime;
use super::skill::SkillLoader;

/// Builder for constructing a [`Runtime`] with providers, tools, and policies.
pub struct RuntimeBuilder {
    handle: RuntimeHandle,
    provider_registry: ProviderRegistry,
}

impl RuntimeBuilder {
    /// Creates a builder with Mentra's builtin tools enabled.
    pub fn new(runtime_intrinsics_enabled: bool) -> Self {
        Self {
            handle: RuntimeHandle::new(runtime_intrinsics_enabled),
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

    /// Registers typed application state that tools can retrieve from their context.
    pub fn with_context(self, context: Arc<dyn Any + Send + Sync>) -> Self {
        self.handle.register_app_context(context);
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

    /// Installs a pre-tool authorization service for runtime tool calls.
    pub fn with_tool_authorizer<A>(self, tool_authorizer: A) -> Self
    where
        A: ToolAuthorizer + 'static,
    {
        Self {
            handle: self.handle.with_tool_authorizer(Arc::new(tool_authorizer)),
            provider_registry: self.provider_registry,
        }
    }

    /// Sets the persisted runtime identifier used to group resumable agents.
    pub fn with_runtime_identifier(self, runtime_identifier: impl Into<Arc<str>>) -> Self {
        Self {
            handle: self.handle.with_runtime_identifier(runtime_identifier),
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
    ///
    /// This is the supported seam for test-time provider injection when you
    /// want to script model responses without live API calls.
    ///
    /// ```rust,no_run
    /// use async_trait::async_trait;
    /// use mentra::{BuiltinProvider, ModelInfo, ProviderDescriptor, Runtime};
    /// use mentra::error::{ProviderError, RuntimeError};
    /// use mentra::provider::{Provider, ProviderEventStream, Request};
    /// use tokio::sync::mpsc;
    ///
    /// struct TestProvider;
    ///
    /// #[async_trait]
    /// impl Provider for TestProvider {
    ///     fn descriptor(&self) -> ProviderDescriptor {
    ///         ProviderDescriptor::new(BuiltinProvider::Anthropic)
    ///     }
    ///
    ///     async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
    ///         Ok(vec![ModelInfo::new("test-model", BuiltinProvider::Anthropic)])
    ///     }
    ///
    ///     async fn stream(
    ///         &self,
    ///         _request: Request<'_>,
    ///     ) -> Result<ProviderEventStream, ProviderError> {
    ///         let (_tx, rx) = mpsc::unbounded_channel();
    ///         Ok(rx)
    ///     }
    /// }
    ///
    /// let runtime = Runtime::empty_builder()
    ///     .with_provider_instance(TestProvider)
    ///     .build()?;
    /// # Ok::<(), RuntimeError>(())
    /// ```
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
