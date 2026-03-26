# Mentra

An agent runtime for building tool-using LLM applications with Rust. It is:

* **Composable**: Mentra gives you a runtime builder, provider abstraction, and
  async tool traits so you can assemble the agent loop you actually want.

* **Controllable**: Builtin policy and authorization hooks let you run
  permissive demos, fail closed, or inspect tool requests before execution.

* **Persistent**: Agents, teams, background work, and runtime state can live in
  a SQLite-backed store instead of disappearing after a single turn.

[![Crates.io][crates-badge]][crates-url]
[![Docs.rs][docs-badge]][docs-url]
[![MIT licensed][mit-badge]][mit-url]
[![Build Status][actions-badge]][actions-url]

[crates-badge]: https://img.shields.io/crates/v/mentra.svg
[crates-url]: https://crates.io/crates/mentra
[docs-badge]: https://img.shields.io/docsrs/mentra
[docs-url]: https://docs.rs/mentra
[mit-badge]: https://img.shields.io/badge/license-MIT-blue.svg
[mit-url]: ./LICENSE
[actions-badge]: https://github.com/WendellXY/mentra/actions/workflows/rust-ci.yml/badge.svg?branch=main
[actions-url]: https://github.com/WendellXY/mentra/actions/workflows/rust-ci.yml

[Crate README](./mentra/README.md) |
[API Docs](https://docs.rs/mentra) |
[Examples](./examples) |
[Issues](https://github.com/WendellXY/mentra/issues)

## Overview

Mentra is a Rust runtime for applications where language models need to reason,
call tools, and keep working across turns. At a high level, it provides a few
major pieces:

* A runtime builder for wiring model providers, persistence, policies, skills,
  and host application state.

* Tool execution primitives, including builtin `shell`, `background_run`,
  `check_background`, `files`, `task`, and team coordination tools.

* Provider integrations for OpenAI, OpenRouter, Anthropic, Gemini, Ollama, and
  LM Studio, with streaming responses and normalized token usage reporting.

* Persistence and coordination for agents, subagents, teams, task boards,
  snapshots, memory compaction, and background notifications.

This repository is a small workspace:

* [`mentra/`](./mentra): the publishable runtime crate.
* [`examples/`](./examples): runnable examples built on top of the runtime.
* [`docs/`](./docs): design notes and feature-specific documentation.

## Example

Add Mentra and Tokio to your `Cargo.toml`:

```toml
[dependencies]
mentra = "0.3.0"
tokio = { version = "1.50.0", features = ["macros", "rt-multi-thread"] }
```

Then, in your `main.rs`:

```rust,no_run
use mentra::{BuiltinProvider, ContentBlock, ModelSelector, Runtime};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = Runtime::builder()
        .with_provider(BuiltinProvider::OpenAI, std::env::var("OPENAI_API_KEY")?)
        .build()?;

    let model = runtime
        .resolve_model(BuiltinProvider::OpenAI, ModelSelector::NewestAvailable)
        .await?;

    let mut agent = runtime.spawn("Assistant", model)?;
    let message = agent
        .send(vec![ContentBlock::text(
            "Summarize why tool-using agents matter.",
        )])
        .await?;

    println!("{}", message.text());
    Ok(())
}
```

More examples can be found in the [`examples/`](./examples) workspace crate,
including:

* [`quickstart`](./examples/quickstart.rs): minimal single-agent setup.
* [`chat`](./examples/chat.rs): interactive, persisted runtime with skills,
  policies, and multiple providers.
* [`custom_tool`](./examples/custom_tool.rs): registering a custom
  `ExecutableTool`.
* [`subagent_tool`](./examples/subagent_tool.rs): disposable subagent
  delegation inside a tool.
* [`team_collaboration`](./examples/team_collaboration.rs): persistent teammate
  workflows.
* [`openai_oauth`](./examples/openai_oauth.rs): OpenAI OAuth-backed provider
  setup.

The builtin runtime shell uses `/bin/sh` on Unix hosts and `cmd.exe` on
Windows hosts. The OpenAI OAuth example keeps `PersistentTokenStoreKind::Auto`
platform-native as well: macOS uses Keychain, while Windows and Linux use the
file-backed store.

## Getting Started

If you want to explore the workspace after cloning the repository, the quickest
path is the example crate.

Run the lightweight quickstart example:

```bash
cargo run -p mentra-examples --example quickstart -- "Summarize the benefits of tool-using agents."
```

Run the richer interactive example:

```bash
cargo run -p mentra-examples --example chat
```

The examples load environment variables from `.env` when available. Set
`OPENAI_API_KEY` for the OpenAI-backed quickstart, or `OPENAI_API_KEY`,
`OPENROUTER_API_KEY`, `ANTHROPIC_API_KEY`, and/or `GEMINI_API_KEY` for the
interactive chat example. You can also set `MENTRA_MODEL` to force a specific
OpenAI model instead of resolving the newest available OpenAI model.

## Getting Help

First, check the [crate README](./mentra/README.md) and the
[API documentation][docs-url]. If you want more implementation detail, the
[`docs/`](./docs) directory includes notes on file operations, memory, shell
safety, and parallel tool calls.

If the answer is not there, please open an issue on the
[issue tracker](https://github.com/WendellXY/mentra/issues).

## Contributing

Thanks for helping improve Mentra.

Before sending changes, run the same checks as CI:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Supported Rust Version

Mentra currently targets Rust 1.85 or newer.

## License

This project is licensed under the [MIT license](./LICENSE).

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in Mentra by you shall be licensed as MIT, without any additional
terms or conditions.
