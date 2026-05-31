# Responses Coding Agent Guide

Mentra's OpenAI and OpenRouter providers use the Responses API by default. The
same provider core can also target xipe-style Responses-compatible endpoints
such as `/backend-api/codex/responses`.

## Recommended Defaults

For manually assembled coding agents, start with these Responses options:

- `ResponsesTransport::HttpSse` for the most stable path.
- `ResponsesTransport::WebSocket` when you want a long-lived connection and the
  endpoint supports `response.create` WebSocket frames.
- `ResponsesStateMode::Hybrid` so Mentra keeps the local transcript as the
  source of truth while opportunistically sending `previous_response_id`.
- `store: Some(false)` unless you explicitly want provider-side storage.
- Function tools are serialized with `strict: false` unless a tool opts into
  `ToolSpec::builder(...).strict(true)`.

`ResponsesStateMode::ReplayOnly` never sends `previous_response_id`.
`ResponsesStateMode::Stateful` sends provider-side state when available and does
not do the hybrid replay fallback if the provider rejects the previous response
id.

## Manual Runtime Example

`Runtime::builder()` registers the builtin coding-agent tools, including
`shell`, `background_run`, `check_background`, and `files`. Shell execution still
requires an explicit runtime policy.

```rust,no_run
use mentra::{
    BuiltinProvider, ContentBlock, ModelSelector, ProviderRequestOptions, ResponsesRequestOptions,
    ResponsesStateMode, ResponsesTransport, Runtime, RuntimePolicy,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = Runtime::builder()
        .with_provider(BuiltinProvider::OpenAI, std::env::var("OPENAI_API_KEY")?)
        .with_policy(RuntimePolicy::permissive())
        .build()?;

    let model = runtime
        .resolve_model(
            BuiltinProvider::OpenAI,
            ModelSelector::Id("gpt-5.4-mini".to_string()),
        )
        .await?;

    let config = mentra::agent::AgentConfig {
        provider_request_options: ProviderRequestOptions {
            responses: ResponsesRequestOptions {
                transport: ResponsesTransport::HttpSse,
                state_mode: ResponsesStateMode::Hybrid,
                store: Some(false),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };

    let mut agent = runtime.spawn_with_config("Coding Agent", model, config)?;
    let reply = agent
        .send(vec![ContentBlock::text(
            "Inspect the repository and explain the test layout.",
        )])
        .await?;

    println!("{}", reply.text());
    Ok(())
}
```

Switch `transport` to `ResponsesTransport::WebSocket` to use the first-class
WebSocket path. Mentra sends each Responses request as a `response.create` frame
with the request fields at the top level:

```json
{
  "type": "response.create",
  "model": "gpt-5.4-mini",
  "instructions": "",
  "input": []
}
```

## xipe-Compatible Provider

Register a custom provider-core instance when your endpoint is xipe-compatible
rather than the default OpenAI API host:

```rust,no_run
use mentra::{
    ModelSelector, ProviderId, ProviderRequestOptions, ResponsesRequestOptions, ResponsesStateMode,
    ResponsesTransport, Runtime,
};

# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
let mut definition = mentra::provider_core::responses::openai_definition();
definition.descriptor.id = ProviderId::new("xipe");
definition.descriptor.display_name = Some("xipe".to_string());
definition.base_url = Some("https://chatgpt.com/backend-api/codex".to_string());

let runtime = Runtime::builder()
    .with_registered_provider(mentra::provider_core::responses::ResponsesProvider::new(
        definition,
        mentra::provider_core::StaticCredentialSource::new(std::env::var("XIPE_TOKEN")?),
    ))
    .build()?;

let model = runtime
    .resolve_model(ProviderId::new("xipe"), ModelSelector::Id("gpt-5.4".to_string()))
    .await?;

let config = mentra::agent::AgentConfig {
    provider_request_options: ProviderRequestOptions {
        responses: ResponsesRequestOptions {
            transport: ResponsesTransport::WebSocket,
            state_mode: ResponsesStateMode::Hybrid,
            store: Some(false),
            ..Default::default()
        },
        ..Default::default()
    },
    ..Default::default()
};
# let _ = (model, config, runtime);
# Ok(())
# }
```

A base URL ending in `/backend-api/codex` targets `/backend-api/codex/responses`.
A base URL ending in `/v1` targets `/v1/responses`. Root OpenAI-compatible base
URLs continue to target `/v1/responses`.

## State Tradeoffs

- Replay-only is easiest to reason about and most recoverable, but every turn is
  pure local transcript replay.
- Hybrid is the recommended coding-agent default: local transcript remains
  authoritative, provider state is used when available, and stale
  `previous_response_id` HTTP failures retry without provider state.
- Stateful is useful only when the endpoint requires provider-side chaining and
  you want failures to surface instead of replaying.
