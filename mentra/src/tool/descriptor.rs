use std::{ops::Deref, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use mentra_provider::{
    ProviderToolKind, ToolLoadingPolicy, ToolSpec as ProviderToolSpec,
    ToolSpecBuilder as ProviderToolSpecBuilder,
};

/// High-level capability labels used for runtime metadata and policy decisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolCapability {
    ReadOnly,
    FilesystemRead,
    FilesystemWrite,
    ProcessExec,
    BackgroundExec,
    TaskMutation,
    TeamCoordination,
    Delegation,
    ContextCompaction,
    SkillLoad,
    Custom(String),
}

/// Declares how much side effect a tool may have when executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ToolSideEffectLevel {
    #[default]
    None,
    LocalState,
    Process,
    External,
}

/// Declares whether a tool call is safe to replay or persist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ToolDurability {
    #[default]
    Ephemeral,
    Persistent,
    ReplaySafe,
}

/// Declares which scheduler/orchestrator lane a tool call belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ToolExecutionCategory {
    ReadOnlyParallel,
    #[default]
    ExclusiveLocalMutation,
    ExclusivePersistentMutation,
    BackgroundJob,
    Delegation,
}

impl ToolExecutionCategory {
    pub fn allows_parallel(self) -> bool {
        matches!(self, Self::ReadOnlyParallel)
    }
}

/// Backward-compatible parallel/exclusive view of execution semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ToolExecutionMode {
    #[default]
    Exclusive,
    Parallel,
}

impl From<ToolExecutionCategory> for ToolExecutionMode {
    fn from(value: ToolExecutionCategory) -> Self {
        if value.allows_parallel() {
            Self::Parallel
        } else {
            Self::Exclusive
        }
    }
}

/// Coarse authorization grouping for runtime policy and review systems.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ToolApprovalCategory {
    #[default]
    Default,
    ReadOnly,
    Filesystem,
    Process,
    Background,
    Delegation,
}

/// Runtime-facing descriptor that wraps the provider-visible tool definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeToolDescriptor {
    pub provider: ProviderToolSpec,
    pub capabilities: Vec<ToolCapability>,
    pub side_effect_level: ToolSideEffectLevel,
    pub durability: ToolDurability,
    pub execution_category: ToolExecutionCategory,
    pub approval_category: ToolApprovalCategory,
    pub execution_timeout: Option<Duration>,
}

impl RuntimeToolDescriptor {
    pub fn builder(name: impl Into<String>) -> RuntimeToolDescriptorBuilder {
        RuntimeToolDescriptorBuilder {
            provider: ProviderToolSpec::builder(name),
            capabilities: Vec::new(),
            side_effect_level: ToolSideEffectLevel::None,
            durability: ToolDurability::Ephemeral,
            execution_category: ToolExecutionCategory::ExclusiveLocalMutation,
            approval_category: ToolApprovalCategory::Default,
            execution_timeout: None,
        }
    }

    pub fn provider_spec(&self) -> &ProviderToolSpec {
        &self.provider
    }
}

impl Deref for RuntimeToolDescriptor {
    type Target = ProviderToolSpec;

    fn deref(&self) -> &Self::Target {
        &self.provider
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeToolDescriptorBuilder {
    provider: ProviderToolSpecBuilder,
    capabilities: Vec<ToolCapability>,
    side_effect_level: ToolSideEffectLevel,
    durability: ToolDurability,
    execution_category: ToolExecutionCategory,
    approval_category: ToolApprovalCategory,
    execution_timeout: Option<Duration>,
}

impl RuntimeToolDescriptorBuilder {
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.provider = self.provider.description(description);
        self
    }

    pub fn input_schema(mut self, input_schema: Value) -> Self {
        self.provider = self.provider.input_schema(input_schema);
        self
    }

    pub fn output_schema(mut self, output_schema: Value) -> Self {
        self.provider = self.provider.output_schema(output_schema);
        self
    }

    pub fn provider_kind(mut self, kind: ProviderToolKind) -> Self {
        self.provider = self.provider.kind(kind);
        self
    }

    pub fn provider_options(mut self, options: Value) -> Self {
        self.provider = self.provider.options(options);
        self
    }

    pub fn loading_policy(mut self, loading_policy: ToolLoadingPolicy) -> Self {
        self.provider = self.provider.loading_policy(loading_policy);
        self
    }

    pub fn defer_loading(mut self, defer_loading: bool) -> Self {
        self.provider = self.provider.defer_loading(defer_loading);
        self
    }

    pub fn capability(mut self, capability: ToolCapability) -> Self {
        self.capabilities.push(capability);
        self
    }

    pub fn capabilities(mut self, capabilities: impl IntoIterator<Item = ToolCapability>) -> Self {
        self.capabilities = capabilities.into_iter().collect();
        self
    }

    pub fn side_effect_level(mut self, side_effect_level: ToolSideEffectLevel) -> Self {
        self.side_effect_level = side_effect_level;
        self
    }

    pub fn durability(mut self, durability: ToolDurability) -> Self {
        self.durability = durability;
        self
    }

    pub fn execution_category(mut self, execution_category: ToolExecutionCategory) -> Self {
        self.execution_category = execution_category;
        self
    }

    pub fn approval_category(mut self, approval_category: ToolApprovalCategory) -> Self {
        self.approval_category = approval_category;
        self
    }

    pub fn execution_timeout(mut self, execution_timeout: Duration) -> Self {
        self.execution_timeout = Some(execution_timeout);
        self
    }

    pub fn build(self) -> RuntimeToolDescriptor {
        RuntimeToolDescriptor {
            provider: self.provider.build(),
            capabilities: self.capabilities,
            side_effect_level: self.side_effect_level,
            durability: self.durability,
            execution_category: self.execution_category,
            approval_category: self.approval_category,
            execution_timeout: self.execution_timeout,
        }
    }
}
