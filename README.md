# Mentra

Mentra is an agent runtime for building tool-using LLM applications.

This repository is a small workspace:

- `mentra/`: the publishable runtime crate
- `examples/`: example programs built on top of the runtime

Consumer-facing crate docs and the canonical quick-start live in [mentra/README.md](mentra/README.md).

## Workspace Commands

Run the interactive example:

```bash
cargo run -p mentra-examples --example chat
```

Run checks:

```bash
cargo check --workspace
cargo test --workspace
```
