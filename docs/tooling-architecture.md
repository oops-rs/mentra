# Mentra Tooling Architecture

Mentra now separates tooling into three explicit layers:

1. `ProviderToolSpec`
   This is the provider-facing contract from `mentra-provider`. It describes the tool name, description, schemas, provider kind, and loading policy. It is the only layer provider adapters should serialize.

2. `RuntimeToolDescriptor`
   This is the Mentra runtime contract in `mentra/src/tool/descriptor.rs`. It wraps a `ProviderToolSpec` and adds runtime-only metadata such as capabilities, side-effect level, durability, execution category, approval category, and timeout policy.

3. `RuntimeTool`
   In code this is expressed as `ToolDefinition + ToolExecutor`. `ToolDefinition::descriptor()` returns the `RuntimeToolDescriptor`. `ToolExecutor` owns execution and authorization preview behavior.

## Orchestration

The runtime scheduler resolves a model-emitted tool call into a `RuntimeToolDescriptor`, builds an authorization preview, runs the authorizer, selects an execution lane, executes the tool, and normalizes the result back into transcript content.

Scheduling is driven by `ToolExecutionCategory`:

- `ReadOnlyParallel`
- `ExclusiveLocalMutation`
- `ExclusivePersistentMutation`
- `BackgroundJob`
- `Delegation`

Only `ReadOnlyParallel` tools are allowed onto the concurrent lane. Everything else is serialized by the orchestrator.

## Adapter Boundary

`mentra-provider` stays provider-neutral. Codex-specific mapping belongs in the adapter layer, not in Mentra core. The Codex adapter should translate Codex tool models into `ProviderToolSpec` plus Mentra runtime descriptors without leaking Codex approval or sandbox semantics into the runtime itself.

## Implementation Notes

- Builtin and intrinsic tools should declare the narrowest correct `ToolExecutionCategory`.
- Read-only tools must implement `ToolExecutor::execute(...)` so the scheduler can run them in parallel safely.
- Mutable-only tools should use `execute_mut(...)`.
- Tests should assert structured payload semantics instead of JSON whitespace formatting when tool results are machine-readable.
