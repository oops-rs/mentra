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
- OpenAI provider support via the Responses API
- image inputs for OpenAI and Anthropic models

## Sending Images

You can attach image blocks alongside text when sending a user turn:

```rust
use mentra::provider::model::ContentBlock;

agent
    .send(vec![
        ContentBlock::text("What is happening in this screenshot?"),
        ContentBlock::image_bytes("image/png", std::fs::read("screenshot.png")?),
    ])
    .await?;
```

For already-hosted assets, use `ContentBlock::image_url(...)` instead.

## Run The Example

Set `OPENAI_API_KEY` or `ANTHROPIC_API_KEY`, then run. The example will let you choose a provider, then show up to 10 models from that provider ordered newest to oldest:

```bash
cargo run -p mentra-examples --example chat
```

## Run Checks

```bash
cargo check --workspace
cargo test --workspace
```
