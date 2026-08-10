# mentra

Mentra is an agent runtime for building tool-using LLM applications.

MSRV: Rust 1.88.

## Current Features

- streaming model response handling
- provider-neutral token usage reporting across OpenAI, OpenRouter, Anthropic, Gemini, Ollama, and LM Studio
- optional tool authorization with structured previews and fail-closed execution blocking
- recoverable malformed tool-call input handling that feeds retry guidance back to the model
- custom tool execution through `ToolDefinition + ToolExecutor`, with `ToolSpec::builder(...)` as the convenience metadata API
- builtin `shell`, `background_run`, `check_background`, and `files` tools
- builtin `task` subagents with isolated child context and parent-side tracking
- persistent agent teams with `team_spawn`, `team_send`, `broadcast`, `team_read_inbox`, and generic request-response protocols via `team_request`, `team_respond`, and `team_list_requests`
- three-layer context compaction with silent tool-result shrinking, auto-summary compaction, and a builtin `compact` tool
- Model Context Protocol servers over stdio and the legacy HTTP+SSE transport, with their tools bridged into the runtime
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

## Custom Compatible Providers

If you need a non-default OpenAI-compatible or Anthropic-compatible endpoint,
register a provider-core instance with a customized `ProviderDefinition`.
Using a distinct provider ID lets you keep the builtin provider alongside your
custom endpoint.

```rust,no_run
use mentra::{ModelSelector, ProviderId, Runtime};

# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
let mut definition = mentra::provider_core::responses::openai_definition();
definition.descriptor.id = ProviderId::new("custom-openai-compatible");
definition.descriptor.display_name = Some("Custom OpenAI-Compatible".to_string());
definition.base_url = Some("https://llm.example.com/".to_string());

let runtime = Runtime::builder()
    .with_registered_provider(mentra::provider_core::responses::ResponsesProvider::new(
        definition,
        mentra::provider_core::StaticCredentialSource::new(std::env::var("CUSTOM_API_KEY")?),
    ))
    .build()?;

let model = runtime
    .resolve_model(
        ProviderId::new("custom-openai-compatible"),
        ModelSelector::NewestAvailable,
    )
    .await?;
# let _ = model;
# Ok(())
# }
```

Anthropic-compatible endpoints follow the same pattern:

```rust,no_run
use mentra::{ProviderId, Runtime};

# fn demo() -> Result<(), Box<dyn std::error::Error>> {
let mut definition = mentra::provider_core::anthropic::definition();
definition.descriptor.id = ProviderId::new("custom-anthropic-compatible");
definition.descriptor.display_name = Some("Custom Anthropic-Compatible".to_string());
definition.base_url = Some("https://claude.example.com/".to_string());

let runtime = Runtime::builder()
    .with_registered_provider(
        mentra::provider_core::anthropic::AnthropicProvider::with_definition_and_credential_source(
            definition,
            mentra::provider_core::StaticCredentialSource::new(std::env::var("CUSTOM_API_KEY")?),
        ),
    )
    .build()?;
# let _ = runtime;
# Ok(())
# }
```

If your compatible endpoint needs different auth or extra headers, mutate the
definition's `auth_scheme`, `headers`, `query_params`, or `retry` fields before
registering it.

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

The builtin local executor is a host executor, not a filesystem or network
sandbox. `RuntimePolicy::permissive()` therefore grants the model the same host
access as the Mentra process. Use it only inside a disposable container or
another boundary you trust. On a normal host, install an OS-enforced custom
executor with `RuntimeBuilder::with_executor(...)`; authorization and shell
validation decide whether a command may start, but they do not contain an
allowed command.

For Responses API transport, xipe-compatible endpoints, and provider-side state
options, see the workspace
[`Responses Coding Agent Guide`](../docs/responses-coding-agent.md).

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
        // Full host shell access. Use only inside a trusted external sandbox.
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
- `RuntimePolicy::workspace_bounded(...)` and `RuntimePolicy::read_only(...)` keep shell execution disabled; their roots constrain builtin file tools and the requested shell working directory, not shell process effects
- builtin shell commands run through `/bin/sh -c` on Unix and `cmd.exe /C` on Windows
- the local executor clears unlisted environment variables and enforces timeouts, output caps, and process-tree cleanup on timeout, but it does not restrict filesystem or network access
- semantic review is opt-in through `RuntimeBuilder::with_tool_authorizer(...)`

Use the default policy when you want a safer runtime surface. Opt into
`RuntimePolicy::permissive()` only when an external sandbox already contains the
entire Mentra process and full host access is intentional.

If you need different command semantics, such as PowerShell on Windows, or
filesystem/network confinement, replace the default local executor with
`RuntimeBuilder::with_executor(...)`. A workspace-bounded or read-only policy
can then explicitly enable foreground and background shell switches; Mentra
treats that executor as a trusted enforcement boundary and does not fall back
to the local executor.

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

Internally, Mentra translates `ToolSpec` into a runtime-only `RuntimeToolDescriptor`, but custom runtime integrations should continue to treat `ToolSpec::builder(...)` as the supported public metadata surface. `ExecutableTool` remains available in this release as a compatibility trait alias over `ToolDefinition + ToolExecutor`.

When a tool needs disposable delegated work, `ParallelToolContext::spawn_subagent()` can create a child agent that inherits the current runtime and model defaults. See the `subagent_tool` example in the workspace examples crate for a complete usage pattern.

Override `ToolExecutor::authorization_preview(...)` when your custom tool needs to expose structured metadata to the installed `ToolAuthorizer`. The default preview includes the resolved working directory, tool capabilities, side-effect level, durability, the raw JSON input, and the same JSON as `structured_input`.

## Tooling Layers

Mentra now separates tool contracts into explicit layers:

- `ProviderToolSpec` in `mentra-provider` for provider-facing serialization
- `RuntimeToolDescriptor` in Mentra for scheduling, approval, and durability metadata
- `ToolDefinition + ToolExecutor` for executable runtime tools

Provider adapters should serialize provider-facing tool specs only. Runtime integrations should continue to implement custom tools with `ToolSpec::builder(...)`, `ToolDefinition`, and `ToolExecutor`.

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

## Model Context Protocol Servers

Mentra connects to external MCP servers and bridges every tool they advertise
into the runtime under a namespaced `mcp__<server>__<tool>` name. Bridged tools
run through the same authorization, result limiter, and paging path as builtin
and custom tools.

Two transports are supported, selected by which configuration type you register.

**stdio** spawns the server as a child process:

```rust,no_run
use mentra::{BuiltinProvider, McpServerConfig, Runtime};

# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
let runtime = Runtime::builder()
    .with_provider(BuiltinProvider::Anthropic, std::env::var("ANTHROPIC_API_KEY")?)
    .with_mcp_server(McpServerConfig {
        name: "filesystem".to_string(),
        command: "npx".to_string(),
        args: vec![
            "-y".to_string(),
            "@modelcontextprotocol/server-filesystem".to_string(),
            "/tmp".to_string(),
        ],
        env: Default::default(),
        cwd: None,
    })
    .build_async()
    .await?;
# let _ = runtime;
# Ok(())
# }
```

**Legacy HTTP+SSE** reaches a hosted server over the network:

```rust,no_run
use mentra::{BuiltinProvider, McpSseServerConfig, Runtime};

# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
let runtime = Runtime::builder()
    .with_provider(BuiltinProvider::Anthropic, std::env::var("ANTHROPIC_API_KEY")?)
    .with_mcp_sse_server(
        McpSseServerConfig::new("observability", "https://mcp.example.com/sse")
            .with_bearer_token(std::env::var("MCP_TOKEN")?),
    )
    .build_async()
    .await?;
# let _ = runtime;
# Ok(())
# }
```

A server that answers `404` on `/mcp` but serves `/sse` needs this transport.

### HTTP+SSE is not Streamable HTTP

`McpSseServerConfig` speaks the transport from MCP protocol revision
`2024-11-05`, which is a different protocol from the newer Streamable HTTP:

| | legacy HTTP+SSE | Streamable HTTP |
|---|---|---|
| Endpoints | a `GET` stream plus a separate `POST` URL | one URL for both |
| POST target | named by the server in an `endpoint` event | the configured URL |
| Responses | always on the `GET` stream | in the POST response or a stream |
| Session | a query parameter in the endpoint URL | the `Mcp-Session-Id` header |

The client opens the configured URL with `Accept: text/event-stream`, waits for
an `endpoint` event naming the POST URL, then posts `initialize`, a
`notifications/initialized` notification, and a paginated `tools/list`. Servers
answer each POST `202 Accepted` and deliver the actual JSON-RPC result as a
`message` event on the stream.

### Security and failure behavior

The endpoint URL is chosen by the server, so it is validated before anything is
sent to it. A resolved endpoint must match the configured URL's scheme, host,
and effective port; a cross-origin endpoint, a protocol-relative `//other.host`
value, embedded credentials, and non-`http(s)` schemes are all refused. Redirects
are never followed on either request.

Configured headers are sent on both the stream and every POST, stored as
`SecretString` so they never appear in `Debug` output, errors, or logs.
Configuring headers against a plaintext `http://` URL on a non-loopback host is
rejected unless `allowing_plaintext_credentials()` is set. No error carries a
response body or SSE payload, so a malicious server cannot write text into your
logs.

Losing the stream ends the session — the client fails closed rather than
hanging, and never reconnects or re-sends a `tools/call`. A call whose response
never arrived surfaces as `McpSseError::RequestIndeterminate`, because the POST
and the response travel on different connections: the tool may have run. Treat
that differently from a rejected POST, which definitely did not execute.

### Using the client directly

Hosts that need their own allowlist, redaction, or evidence policy can drive
`McpSseClient` without registering anything:

```rust,no_run
use mentra::{McpSseClient, McpSseServerConfig};

# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
let config = McpSseServerConfig::new("observability", "https://mcp.example.com/sse")
    .with_bearer_token(std::env::var("MCP_TOKEN")?);

let client = McpSseClient::connect(&config).await?;
for tool in client.tools() {
    println!("{}", tool.name);
}

let result = client
    .call_tool("search_logs", Some(serde_json::json!({"query": "error"})))
    .await?;
println!("{}", result.is_error);

client.shutdown().await;
# Ok(())
# }
```

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
