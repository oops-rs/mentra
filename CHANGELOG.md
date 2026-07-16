# Changelog

## Unreleased

### WS1 — Hygiene

- Remove the orphaned 5,924-line `mentra/src/provider/` source tree; the live
  `mentra/src/provider.rs` adapters remain unchanged.
- Add opt-in `ShellValidationMode::{Off, Warn, Enforce}` policy handling for
  builtin foreground and background shell execution. Validation outcomes map
  to the existing allow/prompt/deny authorization vocabulary, emit existing
  authorization hooks, and enrich shell authorization previews with the
  classifier mode, intent, outcome, and reason.

### WS2 — Generic tool-output truncation

- Limit each successful or error result produced by a builtin, custom, or MCP
  executor to a 2,000-line / 50-KB retained head before it enters the
  transcript and provider request. Truncation preserves complete lines and
  appends an actionable notice; parallel batches limit each result
  independently while retaining call order.
- Spill full oversized output beneath the agent transcript artifact directory
  by default. Structured JSON is never sliced: it is spilled whole and
  replaced by a text pointer. Volatile stores suppress disk spills to preserve
  their no-durable-trace contract.

### WS4 — Thinking and reasoning preservation

- Add provider-neutral, externally tagged `ContentBlock::Thinking` blocks with
  opaque signature/encrypted metadata and exact provider/model/format
  provenance. Stream builders, agent pending turns, persisted transcripts, and
  response-to-event round trips preserve block ordering and metadata.
- Capture and replay Anthropic signed and redacted thinking. Replay is limited
  to assistant history with exact registered-provider and requested-model
  provenance; missing/empty signatures and cross-provider/model history safely
  downgrade to plain text, with a deterministic marker for opaque-only blocks.
- Capture OpenAI Responses reasoning output items, including summaries,
  encrypted content, and reasoning-item IDs. Requests with reasoning enabled
  automatically include `reasoning.encrypted_content` exactly once, and exact
  provider/model replay restores the reasoning input item.
- Preserve Responses reasoning/tool association with local
  `call_id|function_item_id` tool-use IDs only when the response emitted a
  reasoning item. Function-call outputs always project the raw `call_id`, and
  replay omits the function item ID whenever reasoning downgrades across a
  provider or model boundary.
- Backfill Azure-compatible late reasoning ciphertext from terminal
  `response.output` without replacing encrypted content already captured from
  the completed output item or emitting a host reasoning-text delta.
- Emit reasoning text as `AgentEvent::ReasoningDelta` and
  `SessionEvent::AssistantReasoningDelta` while keeping signatures out of host
  deltas. Local compaction summaries exclude thinking from both text extraction
  and the full-transcript JSON prompt.

Gemini thought capture and signatures on `ToolUse`/text blocks remain deferred;
the new neutral representation does not yet provide full Gemini fidelity.

### Compatibility

- Shell validation defaults to `Off`, preserving existing command-execution
  behavior. `RuntimePolicy` only gains private state, so existing constructors
  remain source-compatible; no existing public enum gains a variant.
- Shell `ToolAuthorizationPreview::structured_input` gains an additive
  `validation` object. Consumers comparing that JSON exhaustively must accept
  the new key.
- Results at or below both tool-result limits remain byte-identical. Existing
  results above either new default limit now intentionally become a retained
  head plus notice; the shell stream-capture limit remains separate and still
  applies before this projection boundary.
- `AgentStore` gains a defaulted `allows_disk_artifacts` capability method, so
  existing store implementations continue to compile unchanged.
- Persisted messages and transcripts from before WS4 deserialize unchanged.
  New optional thinking fields are serde-defaulted and omitted when absent.
- Responses streams without a reasoning item retain their historical plain
  `call_id` tool-use IDs; composite IDs are limited to newly preserved
  reasoning/tool associations.
- `ContentBlock`, `ContentBlockStart`, `ContentBlockDelta`, `AgentEvent`, and
  `SessionEvent` gain public variants. Exhaustive matchers must add the new
  reasoning cases (or a deliberate fallback); existing non-exhaustive usage is
  unchanged.

## 0.9.0

### Highlights

- **Per-run round strategy.** `RunOptions::round_strategy` carries an async
  `RoundStrategy` owned by one `Agent::run`, invoked after a committed tool
  round and after a committed tool-free assistant message before the run
  returns. It can continue, inject committed corrective context into the next
  request, switch the next round's model/reasoning, or request a graceful
  (transcript-committing) stop. An absent strategy is byte-identical to the
  previous behavior, and strategy state cannot outlive its run.
- **Structured tool output with termination.** Additive
  `ToolOutput { content, details, terminate }` beside `ToolResult`; defaulted
  `ToolExecutor::execute_output`/`execute_mut_output` bridge every existing
  `Result<String, String>` tool unchanged. `terminate: true` ends the run as
  the value of the tool's own execution (first-class successor to
  `request_idle` for terminal actions). Descriptors gain a `terminal()`
  marker: terminal tools are coerced to exclusive scheduling (never parallel),
  a parallel-lane terminate is rejected as misuse, and calls scheduled after a
  termination receive explicit not-executed error results.
- **Opaque transcript metadata.** `ToolOutput.details` survives the local
  transcript and replay as a per-`tool_use_id` map on `TranscriptItem`
  (`with_details`/`details()`); provider requests only ever receive
  `content`. mentra never interprets the values.
- **Volatile runtime profile.** In-memory `VolatileRuntimeStore` implements
  the full `RuntimeStore` composition so an ephemeral run leaves no durable
  trace — no agent/run rows, transcript upserts, leases, team/task writes, or
  memory ingest artifacts. Isolation on a retained store is an explicit seam
  (fresh construction per run, or `reset()`); the SQLite default store is
  unchanged.
- **Metadata-preserving compaction.** Documented and regression-locked
  guarantee that `details` on preserved and salvaged items survives
  `StandardCompactionEngine::compact` bit-for-bit, and that the
  pre-compaction transcript snapshot carries every item's details.
- **Honest soft budgets.** `RunOptions::token_budget` is a round-boundary
  soft token bound evaluated against reported usage: the crossing round
  completes, the transcript stays committed, and the run stops gracefully —
  never an error, never a rollback, never claimed as a hard cap.
  `RunOptions::child()` derives child options sharing the parent's
  cancellation, stop, deadline, and token accounting. `RoundContext` exposes
  a distinct `transport_retries` counter; existing
  `model_budget`/`model_requests` semantics are unchanged (they count
  provider requests including transient retries).

### Compatibility

- Every seam defaults to current behavior; embedders using descriptor
  builders and `..Default::default()` compile unchanged.
- Source-compat notes for exhaustive constructors/matchers:
  `RuntimeToolDescriptor` gains a public `terminal` field, `RunOptions` gains
  `token_budget`/`token_usage`, and `RuntimeHookEvent::ToolExecutionFinished`
  gains a `details` field — exhaustive literal constructors must add the
  fields and exhaustive struct patterns need `..`.
- Persisted transcripts and agent memory from earlier versions deserialize
  unchanged (`details` is serde-default).

`mentra-provider 0.3.1` is unchanged and does not need to be republished.

## mentra-provider 0.3.1

- Map WebSocket connection failures (`WsError::Io`, `ConnectionClosed`,
  `AlreadyClosed`) to `ProviderError::Retryable` with a 750ms suggested delay
  instead of the terminal `InvalidResponse`/`MalformedStream`, so a consumer's
  whole-turn retry can recover from a transport blip (a dropped SSH tunnel, a
  proxy restart) rather than silently degrading.

## 0.8.0

### Highlights

- Add `Agent::set_reasoning(...)` and `Session::set_reasoning(...)` to change the
  reasoning options requested on future turns (mirrors `set_model`). Enables
  per-phase reasoning effort on a single agent — for example a low effort while
  gathering, then a higher effort for a final synthesis turn — without re-spawning
  and losing the gathered context.
- Add `RunOptions::stop`, a graceful-stop signal distinct from `cancellation`. When
  tripped, the run ends successfully at the next round boundary, **committing** the
  gathered transcript rather than failing and rolling it back the way
  `cancellation` does. Lets a caller stop gathering once enough work is done while
  preserving the context for a follow-up turn on the same agent.

`mentra-provider 0.3.0` is unchanged.

## 0.7.1

### Compatibility

- Update `rusqlite` from 0.32.1 to 0.39 so Mentra can share a single
  `libsqlite3-sys` linkage family with downstream crates using newer sqlite
  bindings.

### Repository Hygiene

- Ignore local `.grapha` graph artifacts.

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
