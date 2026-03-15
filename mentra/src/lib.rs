#![doc = include_str!("../README.md")]

mod default_paths;

/// Agent configuration, lifecycle, and event handling.
pub mod agent;
/// Optional OAuth helpers for provider authentication.
#[cfg(feature = "openai-oauth")]
pub mod auth;
/// Background task coordination types and services.
pub mod background;
/// Working-memory journal and long-term memory services.
pub mod memory;
/// Provider integrations and transport-neutral request/response types.
pub mod provider;
/// Runtime orchestration, persistence, policies, and agent APIs.
pub mod runtime;
/// Team coordination types and collaboration services.
pub mod team;
/// Optional test helpers for deterministic scripted runtimes.
#[cfg(any(test, feature = "test-utils"))]
pub mod test;
/// Tool traits, metadata, and builtin tools.
pub mod tool;

pub use provider::{
    BuiltinProvider, ContentBlock, ImageSource, Message, ModelInfo, ModelSelector,
    ProviderDescriptor, ProviderId, Role,
};

pub use agent::{Agent, AgentConfig};
pub use background::{BackgroundNotification, BackgroundTaskStatus, BackgroundTaskSummary};
pub use runtime::{
    AgentStore, AuditStore, HybridRuntimeStore, LeaseStore, RunStore, Runtime, RuntimePolicy,
    TaskStore,
};
pub use team::{
    TeamDispatch, TeamMemberStatus, TeamMemberSummary, TeamMessage, TeamMessageKind,
    TeamProtocolRequestSummary, TeamProtocolStatus,
};

pub mod error {
    pub use crate::provider::ProviderError;
    pub use crate::runtime::RuntimeError;
}
