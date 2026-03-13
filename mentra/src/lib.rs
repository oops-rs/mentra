#![doc = include_str!("../README.md")]

/// Agent configuration, lifecycle, and event handling.
pub mod agent;
/// Background task coordination types and services.
pub mod background;
/// Provider integrations and transport-neutral request/response types.
pub mod provider;
/// Team coordination types and collaboration services.
pub mod team;
/// Runtime orchestration, persistence, policies, and agent APIs.
pub mod runtime;
/// Tool traits, metadata, and builtin tools.
pub mod tool;

pub use provider::{
    BuiltinProvider, ContentBlock, ImageSource, Message, ModelInfo, ProviderDescriptor, ProviderId,
    Role,
};

pub use agent::{Agent, AgentConfig};
pub use runtime::{Runtime, RuntimePolicy};
pub use background::{BackgroundNotification, BackgroundTaskStatus, BackgroundTaskSummary};
pub use team::{
    TeamDispatch, TeamMemberStatus, TeamMemberSummary, TeamMessage, TeamMessageKind,
    TeamProtocolRequestSummary, TeamProtocolStatus,
};

pub mod error {
    pub use crate::provider::ProviderError;
    pub use crate::runtime::RuntimeError;
}
