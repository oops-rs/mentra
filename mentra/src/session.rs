mod event;
mod handle;
pub(crate) mod hooks;
pub(crate) mod mapping;
pub mod permission;
#[cfg(test)]
mod tests;
mod types;

pub use event::{
    EventSeq, NoticeSeverity, PermissionOutcome, PermissionRuleScope, SessionEvent, TaskKind,
    TaskLifecycleStatus, ToolMutability,
};
pub use handle::{Session, SessionEventReceiver, SessionPermissionHandle, SubagentHandle};
pub use permission::{PermissionDecision, PermissionRequest, RememberedRule, RuleKey, RuleStore};
pub use types::{SessionId, SessionMetadata, SessionStatus};
