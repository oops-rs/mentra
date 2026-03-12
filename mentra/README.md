# mentra

Mentra is an agent runtime for building tool-using LLM applications.

## Current Features

- streaming model response handling
- custom tool execution through the async `ExecutableTool` trait
- builtin `bash`, `background_run`, `check_background`, and `read_file` tools
- builtin `task` subagents with isolated child context and parent-side tracking
- persistent agent teams with `team_spawn`, `team_send`, `broadcast`, `team_read_inbox`, and generic request-response protocols via `team_request`, `team_respond`, and `team_list_requests`
- three-layer context compaction with silent tool-result shrinking, auto-summary compaction, and a builtin `compact` tool
- agent events and snapshots for CLI or UI watchers
- Anthropic provider support
- Gemini Developer API provider support
- OpenAI provider support via the Responses API
- image inputs for OpenAI and Anthropic, plus inline image bytes for Gemini

## Building A Runtime

Use `Runtime::builder()` when you want Mentra's builtin runtime tools, or `Runtime::empty_builder()` when you want to opt into every tool explicitly.

```rust,no_run
use mentra::{BuiltinProvider, runtime::Runtime};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = Runtime::builder()
        .with_provider(BuiltinProvider::OpenAI, std::env::var("OPENAI_API_KEY")?)
        .with_optional_provider(
            BuiltinProvider::Gemini,
            std::env::var("GEMINI_API_KEY").ok(),
        )
        .build()?;

    let _ = runtime;
    Ok(())
}
```

## Coding Agent Setup

`Runtime::builder()` registers Mentra's builtin tools, including `bash`, `background_run`, `check_background`, `read_file`, and the runtime/task/team intrinsics. Shell and background execution remain disabled by default, so coding-agent setups must opt in with a runtime policy.

```rust,no_run
use mentra::{
    BuiltinProvider,
    runtime::{Runtime, RuntimePolicy},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = Runtime::builder()
        .with_provider(BuiltinProvider::OpenAI, std::env::var("OPENAI_API_KEY")?)
        .with_policy(
            RuntimePolicy::default()
                .allow_shell_commands(true)
                .allow_background_commands(true),
        )
        .build()?;

    let _ = runtime;
    Ok(())
}
```

Registering a skills directory also makes the builtin `load_skill` tool available:

```rust,no_run
use mentra::{BuiltinProvider, runtime::Runtime};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = Runtime::builder()
        .with_provider(BuiltinProvider::OpenAI, std::env::var("OPENAI_API_KEY")?)
        .with_skills_dir("./skills")?
        .build()?;

    let _ = runtime;
    Ok(())
}
```

## Sending Images

You can attach image blocks alongside text when sending a user turn:

```rust,no_run
# use mentra::{ContentBlock, runtime::Agent};
# async fn demo(agent: &mut Agent) -> Result<(), Box<dyn std::error::Error>> {
agent
    .send(vec![
        ContentBlock::text("What is happening in this screenshot?"),
        ContentBlock::image_bytes("image/png", std::fs::read("screenshot.png")?),
    ])
    .await?;
# Ok(())
# }
```

For already-hosted assets, use `ContentBlock::image_url(...)` instead. Gemini currently supports inline `image_bytes(...)` inputs only and rejects `image_url(...)`.

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

Set `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or `GEMINI_API_KEY`, then run. The example lets you choose a provider and shows up to 10 models from that provider ordered newest to oldest.

```bash
cargo run -p mentra-examples --example chat
```

## Run Checks

```bash
cargo check --workspace
cargo test --workspace
```
