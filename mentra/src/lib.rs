#![doc = include_str!("../README.md")]

/// Provider integrations and transport-neutral request/response types.
pub mod provider;
/// Runtime orchestration, persistence, policies, and agent APIs.
pub mod runtime;
/// Tool traits, metadata, and builtin tools.
pub mod tool;

pub use provider::{
    BuiltinProvider, ContentBlock, ImageSource, Message, ModelInfo, ProviderDescriptor, ProviderId,
    Role,
};
