# Changelog

## 0.7.0 / mentra-provider 0.3.0

### Highlights

- Add Model Context Protocol client and tool bridge support.
- Add workspace-bounded runtime policy helpers and host sandbox detection.
- Add bash command validation for safer shell-tool execution.
- Add provider-core embedding contracts and a Responses embedding provider.
- Add `Session::set_model(...)` and usage-report events for runtime model
  switching and token accounting.
- Add prompt caching controls for Anthropic requests.
- Add custom provider-core endpoint registration for compatible OpenAI,
  Responses, and Anthropic-style services.

### Responses API

- Add `ResponsesStateMode` with replay-only, hybrid, and stateful modes.
- Add `previous_response_id` tracking and hybrid replay fallback when provider
  state is rejected.
- Add first-class Responses WebSocket transport alongside HTTP/SSE.
- Send xipe-compatible WebSocket `response.create` frames with request fields
  at the top level.
- Default Responses function tools to `strict: false` unless a tool explicitly
  opts into strict mode.
- Refresh `x-codex-turn-state` across HTTP and WebSocket sessions.
- Add a manual coding-agent guide covering Mentra, Responses, xipe, transport
  choices, provider state, and tool strictness.

### Compatibility

- Publish `mentra-provider 0.3.0` before publishing `mentra 0.7.0`.
- Existing flexible built-in shell and file tool schemas remain non-strict by
  default for Responses providers.
- Local transcript replay remains the source of truth in hybrid state mode.

## 0.5.0 / mentra-provider 0.2.0

### Highlights

- Split provider-facing and runtime-facing tool contracts.
- Extracted dedicated tool orchestration and execution-lane scheduling.
- Refactored builtin, files, and intrinsic tooling into thinner facades with
  focused internal modules.

### Compatibility

- `ToolDefinition`, `ToolExecutor`, `ToolSpec`, and `ExecutableTool` remain
  available in this release.
- `ToolSpec::builder(...)` remains the supported convenience API for custom
  tools.
- Provider-visible tool metadata now lives in `mentra-provider`, while runtime
  scheduling and approval metadata stay in `mentra`.

### Migration Notes

- Implement read-only tools with `ToolExecutor::execute(...)`.
- Implement mutating or agent-state-changing tools with
  `ToolExecutor::execute_mut(...)`.
- Publish `mentra-provider 0.2.0` before publishing `mentra 0.5.0`.
