mod command;
mod execution_hooks;
mod hooks;
mod policy;
mod run;
/// Container and sandbox environment detection.
pub mod sandbox;

pub use command::{
    CommandOutput, CommandRequest, CommandSpec, ExecOutput, LocalRuntimeExecutor, RuntimeExecutor,
    read_limited_file,
};
pub use execution_hooks::{
    AfterDecision, BeforeDecision, ExecutionHookParticipant, ExecutionHookRegistration,
    ExecutionHookSnapshot, ExecutionHooks,
};
pub use hooks::{
    AuditHook, AuditLogHook, HookDecision, PostExecutionContext, PostExecutionHook,
    PostExecutionHookRegistration, PostExecutionHooks, PreExecutionContext, PreExecutionHook,
    PreExecutionHookRegistration, PreExecutionHooks, ResultDecision, RuntimeHook, RuntimeHookEvent,
    RuntimeHooks, is_transient_provider_error, is_transient_runtime_error,
};
pub(crate) use policy::ShellValidation;
pub use policy::{RuntimePolicy, ShellValidationMode, normalize_policy_root};
pub use run::{CancellationFlag, CancellationToken, EarlyEnd, ProviderRetry, RunOptions};
