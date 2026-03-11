# Mentra

Mentra is an agent runtime for building tool-using LLM applications.

The repository is organized as a small workspace:

- `mentra/`: the core runtime crate
- `examples/`: example programs built on top of the runtime

## Current Features

- streaming model response handling
- tool execution through an async `ToolHandler` API
- builtin `bash` and `read_file` tools
- builtin `task` subagents with isolated child context and parent-side tracking
- agent events and snapshots for CLI or UI watchers
- Anthropic provider support

## Run The Example

Set `ANTHROPIC_API_KEY`, then run:

```bash
cargo run -p mentra-examples --example chat
```

## Run Checks

```bash
cargo check --workspace
cargo test --workspace
```
