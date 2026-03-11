use std::path::Path;

use crate::{
    provider::{ModelProviderKind, Provider, ProviderRegistry},
    runtime::{RuntimeHandle, error::RuntimeError, skill::SkillLoadError},
    tool::ToolHandler,
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
        T: ToolHandler + 'static,
    {
        self.handle.register_tool(tool);
        self
    }

    pub fn with_skills_dir(self, path: impl AsRef<Path>) -> Result<Self, SkillLoadError> {
        self.handle
            .register_skill_loader(SkillLoader::from_dir(path)?);
        Ok(self)
    }

    pub fn with_optional_provider(
        mut self,
        kind: ModelProviderKind,
        api_key: Option<impl Into<String>>,
    ) -> Self {
        if let Some(api_key) = api_key {
            self.provider_registry
                .register_provider(kind, api_key.into());
        }
        self
    }

    pub fn with_provider(mut self, kind: ModelProviderKind, api_key: impl Into<String>) -> Self {
        self.provider_registry.register_provider(kind, api_key);
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
        if self.provider_registry.providers().is_empty() {
            Err(RuntimeError::ProviderNotFound(None))
        } else {
            Ok(Runtime {
                handle: self.handle,
                provider_registry: self.provider_registry,
            })
        }
    }
}
