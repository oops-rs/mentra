# Mentra

Mentra is an agent runtime for building tool-using LLM applications.

The repository is organized as a small workspace:

- `mentra/`: the core runtime crate
- `examples/`: example programs built on top of the runtime

## Current Features

- streaming model response handling
- tool execution through an async `ToolHandler` API
- builtin `bash`, `background_run`, `check_background`, and `read_file` tools
- builtin `task` subagents with isolated child context and parent-side tracking
- three-layer context compaction with silent tool-result shrinking, auto-summary compaction, and a builtin `compact` tool
- agent events and snapshots for CLI or UI watchers
- Anthropic provider support
- OpenAI provider support via the Responses API
- image inputs for OpenAI and Anthropic models

## Sending Images

You can attach image blocks alongside text when sending a user turn:

```rust
use mentra::ContentBlock;

agent
    .send(vec![
        ContentBlock::text("What is happening in this screenshot?"),
        ContentBlock::image_bytes("image/png", std::fs::read("screenshot.png")?),
    ])
    .await?;
```

For already-hosted assets, use `ContentBlock::image_url(...)` instead.

## Building A Runtime

Use `Runtime::builder()` for the standard builtin tools, or `Runtime::empty_builder()` when you want to opt into tools explicitly:

```rust
use mentra::{ModelProviderKind, runtime::Runtime};

let runtime = Runtime::builder()
    .with_provider(ModelProviderKind::OpenAI, std::env::var("OPENAI_API_KEY")?)
    .build()?;
```

## Context Compaction

Agents compact context by default:

- old tool results are micro-compacted in outbound requests
- when estimated request context exceeds roughly 50k tokens, Mentra writes the full transcript to `.transcripts/` and replaces older history with a model-generated summary
- the model can also call the builtin `compact` tool explicitly

You can tune or disable this per-agent with `ContextCompactionConfig`:

```rust
use mentra::runtime::{AgentConfig, ContextCompactionConfig};

let config = AgentConfig {
    context_compaction: ContextCompactionConfig {
        auto_compact_threshold_tokens: Some(75_000),
        ..ContextCompactionConfig::default()
    },
    ..AgentConfig::default()
};
```

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
