# mentra

Mentra is an agent runtime for building tool-using LLM applications.

MSRV: Rust 1.85.

## Current Features

- streaming model response handling
- provider-neutral token usage reporting across OpenAI, OpenRouter, Anthropic, Gemini, Ollama, and LM Studio
- optional tool authorization with structured previews and fail-closed execution blocking
- recoverable malformed tool-call input handling that feeds retry guidance back to the model
- custom tool execution through the async `ExecutableTool` trait
- builtin `shell`, `background_run`, `check_background`, and `files` tools
- builtin `task` subagents with isolated child context and parent-side tracking
- persistent agent teams with `team_spawn`, `team_send`, `broadcast`, `team_read_inbox`, and generic request-response protocols via `team_request`, `team_respond`, and `team_list_requests`
- three-layer context compaction with silent tool-result shrinking, auto-summary compaction, and a builtin `compact` tool
- agent events and snapshots for CLI or UI watchers
- Anthropic provider support
- Gemini Developer API provider support
- OpenAI provider support via the Responses API
- OpenRouter provider support via the Responses API
- Ollama provider support via the OpenAI-compatible Responses API
- LM Studio provider support via the OpenAI-compatible Responses API
- image inputs for OpenAI and Anthropic, plus inline image bytes for Gemini

## Quickstart Example

Clone the repository and run the workspace quickstart example:

```bash
cargo run -p mentra-examples --example quickstart -- "Summarize the benefits of tool-using agents."
```

The quickstart example accepts a prompt from CLI args or stdin. Set `MENTRA_MODEL` to force a specific OpenAI model; otherwise it resolves the newest available OpenAI model automatically.

## Building A Runtime

Use `Runtime::builder()` when you want Mentra's builtin runtime tools, or `Runtime::empty_builder()` when you want to opt into every tool explicitly.

```rust,no_run
use mentra::{BuiltinProvider, Runtime};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = Runtime::builder()
        .with_provider(BuiltinProvider::OpenAI, std::env::var("OPENAI_API_KEY")?)
        .with_optional_provider(
            BuiltinProvider::OpenRouter,
            std::env::var("OPENROUTER_API_KEY").ok(),
        )
        .with_optional_provider(
            BuiltinProvider::Gemini,
            std::env::var("GEMINI_API_KEY").ok(),
        )
        .with_ollama()
        .with_lmstudio()
        .build()?;

    let _ = runtime;
    Ok(())
}
```

`with_ollama()` targets `http://127.0.0.1:11434/` and `with_lmstudio()` targets
`http://127.0.0.1:1234/`, using each server's OpenAI-compatible API surface.

## Architecture

Mentra is organized around four runtime subsystems:

- execution: model providers, runtime policy, hooks, turn execution, and shell/background command routing
- persistence: agent records, run state, task snapshots, leases, team state, background notifications, and memory
- tooling: builtin and custom tools, optional skills, and typed app context
- collaboration: persistent teammates, team inbox/request flows, and background task wakeups

Persistent teammates are hosted as async actors on a shared Tokio runtime. Live actors are wake-driven rather than steady-state polled: inbox appends, protocol updates, background task completion, explicit resume, and autonomy timers wake the actor to process durable state already written to the store. After a restart, the persisted team inbox, protocol requests, and background notifications remain the source of truth, and `Runtime::resume(...)` revives teammate actors against that stored state.

## Resolving A Model

Use `Runtime::resolve_model(...)` when you want provider-aware model selection without reimplementing discovery or `ModelInfo` construction in application code.

```rust,no_run
use mentra::{BuiltinProvider, ModelSelector, Runtime};

# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
let runtime = Runtime::builder()
    .with_provider(BuiltinProvider::OpenAI, std::env::var("OPENAI_API_KEY")?)
    .build()?;
let model = runtime
    .resolve_model(
        BuiltinProvider::OpenAI,
        std::env::var("MENTRA_MODEL")
            .map(ModelSelector::Id)
            .unwrap_or(ModelSelector::NewestAvailable),
    )
    .await?;

let _ = model;
# Ok(())
# }
```

## Coding Agent Setup

`Runtime::builder()` registers Mentra's builtin tools, including `shell`, `background_run`, `check_background`, `files`, and the runtime/task/team intrinsics. Shell and background execution remain disabled by default, so coding-agent setups must opt in with a runtime policy. If you want semantic review before tools execute, install a `ToolAuthorizer`.

```rust,no_run
use async_trait::async_trait;
use mentra::{BuiltinProvider, Runtime, RuntimePolicy};
use mentra::tool::{
    ToolAuthorizationDecision, ToolAuthorizationRequest, ToolAuthorizer,
};

struct AllowAllAuthorizer;

#[async_trait]
impl ToolAuthorizer for AllowAllAuthorizer {
    async fn authorize(
        &self,
        _request: &ToolAuthorizationRequest,
    ) -> Result<ToolAuthorizationDecision, mentra::error::RuntimeError> {
        Ok(ToolAuthorizationDecision::allow())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = Runtime::builder()
        .with_provider(BuiltinProvider::OpenAI, std::env::var("OPENAI_API_KEY")?)
        .with_policy(RuntimePolicy::permissive())
        .with_tool_authorizer(AllowAllAuthorizer)
        .build()?;

    let _ = runtime;
    Ok(())
}
```

## Runtime Policy Defaults

Mentra's builtin runtime tools are available by default, but command execution is not:

- `Runtime::builder()` registers the builtin shell, background, file, task, team, and memory-oriented intrinsics
- foreground shell execution is disabled by default
- background command execution is disabled by default
- `RuntimePolicy::permissive()` enables both shell and background command execution
- builtin shell commands run through `/bin/sh -c` on Unix and `cmd.exe /C` on Windows
- runtime policy still enforces hard limits such as working-directory roots, file read/write roots, allowed environment variables, timeouts, output caps, and background task limits
- semantic review is opt-in through `RuntimeBuilder::with_tool_authorizer(...)`

Use the default policy when you want a safer runtime surface, and opt into `RuntimePolicy::permissive()` only when you are intentionally building a coding-agent or automation workflow that should be able to act on the local workspace.

If you need different command semantics, such as PowerShell on Windows or a sandboxed executor, replace the default local executor with `RuntimeBuilder::with_executor(...)`.

## Tool Authorization

Mentra can run a caller-provided authorization pass before any tool executes. This is the recommended integration point for LLM-based security review, human approval, or custom policy engines.

- no authorizer installed: tools run under the remaining hard runtime constraints
- authorizer returns `Allow`: the tool executes
- authorizer returns `Prompt` or `Deny`: Mentra blocks execution and returns an error `tool_result`
- authorizer timeout or error: Mentra fails closed and blocks execution

Every authorization request includes a `ToolAuthorizationPreview` with tool metadata plus structured input. Builtin tools provide more specific previews:

- `shell` and `background_run` include the raw command, resolved working directory, timeout, background flag, and justification
- `files` includes resolved paths and operation kinds such as `read`, `search`, `set`, `move`, and `delete`, without file contents

```rust,no_run
use async_trait::async_trait;
use mentra::tool::{
    ToolAuthorizationDecision, ToolAuthorizationRequest, ToolAuthorizer,
};

struct DenyDeletes;

#[async_trait]
impl ToolAuthorizer for DenyDeletes {
    async fn authorize(
        &self,
        request: &ToolAuthorizationRequest,
    ) -> Result<ToolAuthorizationDecision, mentra::error::RuntimeError> {
        let structured = &request.preview.structured_input;
        let denies_delete = structured
            .get("operations")
            .and_then(|value| value.as_array())
            .is_some_and(|ops| ops.iter().any(|op| op.get("op").and_then(|v| v.as_str()) == Some("delete")));

        if request.tool_name == "files" && denies_delete {
            Ok(ToolAuthorizationDecision::deny("delete operations require manual approval"))
        } else {
            Ok(ToolAuthorizationDecision::allow())
        }
    }
}
```

Registering a skills directory also makes the builtin `load_skill` tool available:

```rust,no_run
use mentra::{BuiltinProvider, Runtime};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = Runtime::builder()
        .with_provider(BuiltinProvider::OpenAI, std::env::var("OPENAI_API_KEY")?)
        .with_skills_dir("./skills")?
        .build()?;

    let _ = runtime;
    Ok(())
}
```

## App Context

If your tools need access to typed host-side state, register it on the runtime and retrieve it from `ToolContext` or `ParallelToolContext`:

```rust,no_run
use std::sync::Arc;

use async_trait::async_trait;
use mentra::{
    BuiltinProvider, Runtime,
    tool::{ToolContext, ToolDefinition, ToolExecutor, ToolResult, ToolSpec},
};
use serde_json::{Value, json};

struct AppState {
    api_base: String,
}

struct InspectStateTool;

impl ToolDefinition for InspectStateTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("inspect_state")
            .description("Return the configured API base URL.")
            .input_schema(json!({
                "type": "object",
                "properties": {}
            }))
            .build()
    }
}

#[async_trait]
impl ToolExecutor for InspectStateTool {
    async fn execute_mut(&self, ctx: ToolContext<'_>, _input: Value) -> ToolResult {
        let state = ctx.app_context::<AppState>()?;
        Ok(state.api_base.clone())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = Runtime::builder()
        .with_provider(BuiltinProvider::OpenAI, std::env::var("OPENAI_API_KEY")?)
        .with_context(Arc::new(AppState {
            api_base: "https://api.example.com".to_string(),
        }))
        .with_tool(InspectStateTool)
        .build()?;

    let _ = runtime;
    Ok(())
}
```

## Custom Tools

Use `ToolSpec::builder(...)` to define custom tools without hand-assembling the metadata struct:

```rust,no_run
use async_trait::async_trait;
use mentra::tool::{
    ParallelToolContext, ToolCapability, ToolDefinition, ToolDurability, ToolExecutor,
    ToolResult, ToolSideEffectLevel, ToolSpec,
};
use serde_json::{Value, json};

struct UppercaseTool;

impl ToolDefinition for UppercaseTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("uppercase_text")
            .description("Uppercase the provided text")
            .input_schema(json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" }
                },
                "required": ["text"]
            }))
            .capability(ToolCapability::ReadOnly)
            .side_effect_level(ToolSideEffectLevel::None)
            .durability(ToolDurability::ReplaySafe)
            .execution_timeout(std::time::Duration::from_secs(5))
            .build()
    }
}

#[async_trait]
impl ToolExecutor for UppercaseTool {
    async fn execute(&self, _ctx: ParallelToolContext, input: Value) -> ToolResult {
        let text = input
            .get("text")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "text is required".to_string())?;
        Ok(text.to_uppercase())
    }
}
```

`ToolSpec::execution_timeout(...)` is enforced by Mentra around the tool future itself, which is useful for network-backed tools that need a tighter budget than the overall agent run.

When a tool needs disposable delegated work, `ParallelToolContext::spawn_subagent()` can create a child agent that inherits the current runtime and model defaults. See the `subagent_tool` example in the workspace examples crate for a complete usage pattern.

Override `ToolExecutor::authorization_preview(...)` when your custom tool needs to expose structured metadata to the installed `ToolAuthorizer`. The default preview includes the resolved working directory, tool capabilities, side-effect level, durability, the raw JSON input, and the same JSON as `structured_input`.

## Hosted Tool Search

Mentra can mark custom tools as deferred and let a provider load them on demand with native hosted tool search.

Mark a tool as deferred in its `ToolSpec`:

```rust,no_run
use async_trait::async_trait;
use mentra::tool::{ParallelToolContext, ToolDefinition, ToolExecutor, ToolResult, ToolSpec};
use serde_json::{Value, json};

struct LookupOrderTool;

impl ToolDefinition for LookupOrderTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("lookup_order")
            .description("Look up an order by id.")
            .input_schema(json!({
                "type": "object",
                "properties": {
                    "order_id": { "type": "string" }
                },
                "required": ["order_id"]
            }))
            .defer_loading(true)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for LookupOrderTool {
    async fn execute(&self, _ctx: ParallelToolContext, _input: Value) -> ToolResult {
        Ok("order loaded".to_string())
    }
}
```

Enable hosted tool search per agent with `ProviderRequestOptions`:

```rust,no_run
use mentra::agent::AgentConfig;
use mentra::provider::{ProviderRequestOptions, ReasoningEffort, ReasoningOptions, ToolSearchMode};

let config = AgentConfig {
    provider_request_options: ProviderRequestOptions {
        tool_search_mode: ToolSearchMode::Hosted,
        reasoning: Some(ReasoningOptions {
            effort: Some(ReasoningEffort::Medium),
            summary: None,
        }),
        ..Default::default()
    },
    ..Default::default()
};
```

Current provider support:

- OpenAI: supported through the Responses API hosted `tool_search` surface
- Anthropic: supported through the Messages API BM25 tool-search server tool
- Gemini: deferred custom tools are not supported; Mentra returns `InvalidRequest`

Reasoning effort support:

- OpenAI and OpenRouter: Mentra forwards `provider_request_options.reasoning.effort` as Responses API reasoning effort
- Anthropic: Mentra maps unified reasoning effort to adaptive thinking on Claude 4.6 models
- Gemini: Mentra maps unified reasoning effort to `thinkingLevel` on Gemini 3 models
- Anthropic models older than 4.6 and Gemini models older than 3 return `InvalidRequest` when unified reasoning effort is set

Deferred tools are filtered through `ToolProfile` just like immediate tools. If you force a deferred tool with `ToolChoice::Tool { name }`, Mentra serializes that specific tool as immediate for the request so explicit invocation still works.

## Tool Profiles

Register tools once on the runtime, then use `AgentConfig::tool_profile` to expose different subsets for different operating modes.

```rust,no_run
use mentra::{BuiltinProvider, ModelSelector, Runtime};
use mentra::agent::{AgentConfig, ToolProfile};

# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
let runtime = Runtime::builder()
    .with_provider(BuiltinProvider::OpenAI, std::env::var("OPENAI_API_KEY")?)
    .build()?;
let model = runtime
    .resolve_model(
        BuiltinProvider::OpenAI,
        ModelSelector::Id("gpt-5.4-mini".to_string()),
    )
    .await?;

let queue_mode = AgentConfig {
    tool_profile: ToolProfile::only([
        "shell",
        "background_run",
        "check_background",
        "files",
        "task",
    ]),
    ..Default::default()
};

let direct_mode = AgentConfig {
    tool_profile: ToolProfile::hide(["task", "background_run"]),
    ..Default::default()
};

let _queue_agent = runtime.spawn_with_config("Queue Agent", model.clone(), queue_mode)?;
let _direct_agent = runtime.spawn_with_config("Direct Agent", model, direct_mode)?;
# Ok(())
# }
```

This is the recommended pattern when one application needs multiple tool surfaces such as a queue-backed agent with delegation enabled and a direct mode that keeps the same runtime but hides long-running or task-oriented tools.

## CLI Integration Pattern

For CLI-style coding or analysis tools, the usual setup is:

- register a superset of builtin and custom tools on one runtime
- scope shell and file access with `RuntimePolicy`
- keep application-specific output paths in app context for custom tools
- switch behavior per mode by changing `AgentConfig::tool_profile`, not by rebuilding the runtime
- inspect `agent.history()` after the run when you want to render a compact tool log or transcript summary

The `cli_runtime` example in the workspace examples crate shows this pattern end to end with custom tools, policy setup, mode-specific tool surfaces, and transcript inspection.

## Disposable Tasks vs Persistent Teams

Mentra supports two different delegation models:

- use the builtin `task` tool or `ParallelToolContext::spawn_subagent()` for short-lived disposable delegation that should return a single summary to the parent
- use `team_spawn`, `team_send`, `team_read_inbox`, `team_request`, and `team_respond` when you want a persistent teammate with a durable mailbox and request/response workflow across turns

The `task` path is ideal for one-off decomposition inside a single run. The `team_*` tools are for longer-lived collaborators that should keep state, receive follow-up work, and participate in approval or shutdown flows.

## Sending Images

You can attach image blocks alongside text when sending a user turn:

```rust,no_run
# use mentra::{ContentBlock, Agent};
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

## Long-Term Memory

Agents automatically recall from long-term memory by default. When you use `Runtime::builder()`, the builtin runtime intrinsics include:

- `memory_search` for explicit recall
- `memory_pin` for writing important facts
- `memory_forget` for tombstoning a specific memory record

`MemoryConfig` controls recall and write behavior per agent. The default configuration enables automatic recall and memory write tools, which is useful for long-running assistants and teammate workflows. Disable write tools when you want recall without model-initiated mutation.

## Context Compaction

Agents compact context by default:

- old tool results are micro-compacted in outbound requests
- when estimated request context exceeds roughly 50k tokens, Mentra writes the full transcript to the default transcript directory and replaces older history with a model-generated summary
- the model can also call the builtin `compact` tool explicitly

You can tune or disable this per-agent with `CompactionConfig`:

```rust
use mentra::agent::{AgentConfig, CompactionConfig};

let config = AgentConfig {
    compaction: CompactionConfig {
        auto_compact_threshold_tokens: Some(75_000),
        ..Default::default()
    },
    ..Default::default()
};
```

## Data And Persistence Defaults

For non-test builds, Mentra keeps all default persisted state under a workspace-scoped app-data directory:

- store: `<platform data dir>/mentra/workspaces/<workspace-hash>/runtime.sqlite`
- runtime-scoped stores: `<platform data dir>/mentra/workspaces/<workspace-hash>/runtime-<runtime-id>.sqlite`
- team state: `<platform data dir>/mentra/workspaces/<workspace-hash>/team/`
- task state: `<platform data dir>/mentra/workspaces/<workspace-hash>/tasks/`
- transcripts: `<platform data dir>/mentra/workspaces/<workspace-hash>/transcripts/`

If the platform data directory cannot be resolved, Mentra falls back to `.mentra/workspaces/<workspace-hash>/...` inside the current workspace.

Override these defaults when needed:

- use `Runtime::builder().with_store(...)` for the SQLite store
- customize `AgentConfig::task.tasks_dir`, `AgentConfig::team.team_dir`, and `AgentConfig::compaction.transcript_dir` for task, team, and transcript storage

## Persistence Extension Points

The public persistence surface is intentionally split into narrower traits:

- `AgentStore` for agent records and working-memory snapshots
- `RunStore` for turn and run lifecycle tracking
- `TaskStore` for the dependency-aware task board
- `LeaseStore` for runtime ownership and resume coordination

`RuntimeStore` composes those traits with `TeamStore`, `BackgroundStore`, and `MemoryStore`. `SqliteRuntimeStore` is the default all-in-one backend. `HybridRuntimeStore` keeps SQLite runtime state and swaps in the hybrid memory engine for richer long-term memory behavior.

## Testing With MockRuntime

Enable the `test-utils` feature when you want a deterministic scripted runtime for unit and integration tests.

`mentra::test::MockRuntime` wraps a real runtime with:

- a scripted provider
- a temporary SQLite-backed runtime store
- deterministic per-turn helper methods for assistant text, streamed text, tool-call turns, and provider failures

This is the recommended way to test Mentra-based agents and tools without live API keys.

The common pattern is:

- build a `MockRuntime`
- register the same custom tools you use in production
- spawn an agent with the `AgentConfig` or `ToolProfile` you want to verify
- assert against `mock.recorded_requests()` to confirm the runtime exposed the expected tools and tool-choice hints

See `mentra::test` and the crate tests for a full example of asserting runtime assembly with custom tools and filtered tool surfaces.

## Interactive Repo Example

Clone the repository when you want the richer interactive demo with provider selection, persisted runtime inspection, skills loading, and team/task visibility.

Set `OPENAI_API_KEY`, `OPENROUTER_API_KEY`, `ANTHROPIC_API_KEY`, or `GEMINI_API_KEY`, then run. The example lets you choose a provider and shows up to 10 models from that provider ordered newest to oldest.

```bash
cargo run -p mentra-examples --example chat
```

Additional focused examples live in the same crate:

```bash
cargo run -p mentra-examples --example custom_tool
cargo run -p mentra-examples --example subagent_tool
cargo run -p mentra-examples --example team_collaboration
cargo run -p mentra-examples --example cli_runtime -- --mode direct
```

`cli_runtime` is the closest example to a real integration. It combines runtime policy setup, custom tools, mode-specific `ToolProfile` selection, and transcript inspection after the run.

## Run Checks

```bash
cargo fmt --all --check
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
