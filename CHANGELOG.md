# Changelog

## 0.14.0

### A session turn can carry run options

- `Session::append_turn_with_options` and `resume_turn_with_options` take
  `RunOptions`, so a turn driven through a session can be cancelled, given a
  deadline, or bounded by a tool budget. Previously only `Agent::run` accepted
  them and `Session` hardcoded `RunOptions::default()`, which left a host with
  a stop button no way to build one without dropping to `Agent` and giving up
  the session event stream and permission handle ([#10](https://github.com/oops-rs/mentra/issues/10)).
- `append_turn` and `resume_turn` are unchanged, now delegating with default
  options. Both paths share one internal helper, so a prompted turn and a
  resumed turn cannot report their status, turn count, or events differently.
- `CancellationToken` implements `Debug`, so a host embedding one in its own
  options struct can still derive `Debug` on it ([#11](https://github.com/oops-rs/mentra/issues/11)).

### Mock runtimes can exercise permissions

- `MockRuntimeBuilder::with_tool_authorizer` installs an authorizer on the
  scripted runtime. Without one the session authorizer allows every call
  unconditionally and `PermissionRequested` never fires, so the permission
  flow — the one whose failure mode is a hang rather than an error — could not
  be tested against a mock at all. Consumers were hand-building a runtime
  around a scripted provider instead ([#12](https://github.com/oops-rs/mentra/issues/12)).
- `ToolAuthorizer` is implemented for `Box<T>` and `Arc<T>`, so an authorizer
  chosen at runtime can be passed to anything taking `impl ToolAuthorizer`.

## 0.13.0

### Conversations branch

- Transcript entries carry `id` and `parent_id`, making a conversation a tree
  with one active path. `Session::branch_from` returns to an earlier entry so
  the next turn takes a different path; entries left behind move off the
  active path but stay in the transcript, reachable through
  `Session::children`, so an abandoned line of work can be returned to.
  Branching is a move of the leaf, not a copy of history.
- `AgentTranscript::items` still returns the active path root-to-leaf, so
  existing callers are unaffected. New: `leaf`, `entry`, `children`,
  `archived`, `branch_from`, plus `EntryId` and `BranchError`.
- `SessionEvent::Branched` reports the move and how many entries left the path.
- Transcripts written before entries had ids deserialize unchanged and have
  their chain linked on load, so branching works on existing sessions.
- Compaction preserves a salvaged entry's identity and content but re-derives
  its parent link, because compaction genuinely moves the entry.

### Skills

- `register_skills_dir` is additive. Registering a second root used to discard
  the first silently; roots now layer, with an earlier root shadowing a later
  one by name. `register_skills_dirs` takes several at once, strongest first.
  A repeated name *within* one root is still an error.
- `Runtime::skills` lists what loaded — name, description, and source path —
  so a host can show a skill set, expose it as commands, or assert on it in a
  test. Bodies stay behind `load_skill`.
- `SkillLoadError` is re-exported from `runtime` and the crate root. It was
  public inside a private module, so callers could not name it, store it, or
  match on `DuplicateSkillName`.

### Compaction

- `CompactionSummary` carries `files_touched`, seeded from the previous
  summary and unioned with newly extracted paths. Previously the file list
  lived only in the summarization prompt, so it decayed out of context after a
  few rounds and the agent silently stopped knowing what it had edited.
- A turn pinned by a tool call and its result is now summarized as a unit
  instead of refused. Compaction used to return `Ok(None)` when that pair was
  the whole transcript, leaving an over-budget turn unrecoverable at exactly
  the moment compaction was needed. A bare user turn is excluded: replacing
  the user's only instruction with a summary of itself discards the thing the
  turn exists to convey.

### Fixes

- `SessionEvent::ToolCompleted` names its tool. The field was always empty,
  because `ContentBlock::ToolResult` carries only `tool_use_id` — making
  completion, where failures surface, the one lifecycle event a client could
  not attribute without keeping its own id-to-name map.
- Legacy MCP HTTP+SSE errors no longer retain server-controlled JSON-RPC
  messages or data, endpoint origins or schemes, response content types, or
  response-decode diagnostics. JSON-RPC codes and fixed failure classifications
  remain available, while intentional MCP tool-result content is unchanged.

## 0.12.0

### Model Context Protocol over HTTP+SSE

- Add a native client for the legacy MCP HTTP+SSE transport (protocol revision
  `2024-11-05`), for servers that answer `404` on `/mcp` and only serve `/sse`.
  This is not Streamable HTTP: the client holds a long-lived `GET` stream open,
  waits for an `endpoint` event naming a separate `POST` URL, and reads JSON-RPC
  results back off the stream rather than from the `POST` response.
- Add `McpSseServerConfig`, `McpSseClient`, `McpSseLimits`, `McpSseError`, and
  `SecretString`, plus `McpManager::connect_sse` and
  `RuntimeBuilder::with_mcp_sse_server` / `with_mcp_sse_servers`. Bridged SSE
  tools are namespaced and pass through the same authorization, result limiter,
  and paging path as builtin and custom tools.
- `McpSseClient` is usable directly, without registering anything, so a host can
  apply its own allowlist, redaction, and evidence policy before a model sees a
  tool.
- `McpServerConfig` remains the stdio configuration type and gains no transport
  field; `with_mcp_server`/`with_mcp_servers` are unchanged.
  `McpBridgedTool::new` is now generic over the transport and still accepts an
  `Arc<McpStdioClient>`.

### Transport security

- The server names the `POST` endpoint, so it is validated before anything is
  sent: the resolved URL must match the configured stream URL on scheme, host,
  and effective port. Cross-origin endpoints, protocol-relative `//host` values,
  embedded credentials, and non-`http(s)` schemes are refused, and redirects are
  never followed on either request.
- Configured headers are sent on both the stream and every `POST`, held as
  `SecretString` so they cannot reach `Debug` output, errors, or logs.
  `SecretString` deliberately does not implement `Serialize`, so serializing a
  config that holds one is a compile error rather than a credential written to
  disk. Headers over plaintext `http://` to a non-loopback host are rejected
  unless `allow_plaintext_credentials` is set.
- No error variant carries a response body or SSE payload, so a malicious server
  cannot inject text into an operator's logs. SSE events, the `endpoint` event,
  and diagnostic bodies are separately size-bounded.
- Losing the stream fails closed. The transport never reconnects and never
  re-sends a `tools/call`; an accepted-but-unanswered call surfaces as
  `McpSseError::RequestIndeterminate`, distinct from a rejected `POST`, because
  the request and its response travel on different connections and the tool may
  have executed.

### MCP fixes

- Stop the stdio client leaking a pending-map entry when a request times out or
  its write fails.
- Stop the stdio client resolving a caller with `null` when the server sends its
  own request, such as `ping`, reusing that id. Responses are now recognized by
  carrying a result or an error.
- Bound `tools/list` pagination on both transports. Cursors are opaque, so a
  server repeating one is only stoppable by a page bound; previously it looped
  forever, accumulating tools without limit.
- Abort the SSE reader task when an endpoint is refused, rather than leaking the
  task and its connection.

### Breaking

- `McpManager::call_tool` now returns `Result<McpToolCallResult, String>` instead
  of `Result<McpToolCallResult, McpClientError>`. The two transports report
  failures with different error enums and the manager only ever renders the
  message. Callers that matched on `McpClientError` should match on the message
  or use the transport client directly.

### Runtime safety

- Keep `RuntimePolicy::workspace_bounded(...)` and `RuntimePolicy::read_only(...)`
  shell execution disabled by default. Their path roots constrain builtin file
  tools and the requested working directory; they cannot confine filesystem or
  network effects of the host `LocalRuntimeExecutor`. Hosts may explicitly
  enable shell switches after installing an OS-enforced executor through
  `RuntimeBuilder::with_executor(...)`.
- Document `RuntimePolicy::permissive()` as full host shell access and separate
  semantic authorization from OS-enforced containment.

## 0.11.0

### Automatic tool-result paging

- Add opt-in `AgentConfig.tool_result_paging` (`ToolResultPagingConfig`,
  default threshold 64 KiB / page 32 KiB). An oversized tool result inserts
  only its first window into the transcript, cut on a line boundary with an
  honest trailer naming the follow-up call; `None` (the default) preserves
  existing behavior byte-identically.
- Add the built-in `read_tool_result(tool_use_id, start_line)` tool: serves
  further windows with absolute line numbers from full results retained per
  agent, registered runtime-wide and offered only to agents with paging
  enabled. It never re-executes the original tool, never pages its own
  windows, and cannot express a cross-agent read.
- `AgentEvent::ToolExecutionFinished` continues to carry the complete,
  unpaged result: paging shapes the model's view, never the event record,
  so evidence/ledger consumers observe no change.
- Paging runs downstream of the universal tool-result limiter; consumers
  enabling it should raise `max_tool_result_bytes`/`max_tool_result_lines`
  to whatever a tool may legitimately return and keep the limiter as the
  anti-abuse backstop. Design and as-built notes: `docs/tool-result-paging.md`.

### Team

- Make team protocol request ids collision-resistant.

## mentra-provider 0.4.1

- Tolerate Responses stream envelopes that omit `id` and `model`. A Codex-style
  proxy synthesizes its own terminal `response.failed` / `response.incomplete`
  frames when the upstream errors, and those frames can carry only the error
  body. Both fields were required, so the frame failed to deserialize and the
  turn died with `missing field model` — masking the provider error the frame
  was carrying. The real error now surfaces, and an incomplete response still
  terminates the message.
- Fall back to the requested model in `MessageStarted` when a `response.created`
  frame omits `model`, instead of reporting an empty model string.

## 0.10.0 / mentra-provider 0.4.0

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
  executor to a 2,000-line / 50-KiB retained head before it enters the
  transcript and provider request. Truncation preserves complete lines and
  appends an actionable notice; parallel batches limit each result
  independently while retaining call order.
- Spill full oversized output beneath the agent transcript artifact directory
  by default. Structured JSON is never sliced: when spill succeeds, it is
  written whole and replaced by a text pointer; otherwise the replacement
  notice explains why the full output was not saved. Volatile stores suppress
  disk spills to preserve their no-durable-trace contract.

### WS3 — Model-conventional coding tools

- Add opt-in `read`, `ls`, `grep`, `glob`, `write`, and `edit` builtin tools as
  thin executors over the existing transactional `WorkspaceEditor`. Read-only
  tools use the parallel lane; writes and edits use the exclusive local-mutation
  lane and retain the same runtime-policy checks as the batched `files` tool.
- Add `FileToolProfile::{Batched, Split, Both}` and
  `RuntimeBuilder::with_file_tools(...)`. The default remains `Batched`; builder
  calls reconfigure the eagerly populated registry immediately, and `Both`
  exposes both model-facing surfaces over one engine.
- Add recursive globbing plus grep file filters, literal and case-insensitive
  modes, context, multiline regular expressions, and a Unicode-safe
  500-character cap per rendered physical line.
- Harden the split `edit` tool with original-content multi-edit validation,
  overlap/uniqueness/no-op guards, BOM and CRLF restoration, and NFKC/smart
  punctuation fuzzy matching. Provider-visible content stays a short summary;
  display diff, unified patch, and `first_changed_line` remain local
  `ToolOutput::details` metadata.
- Reauthorize every descendant reached by recursive list/search/glob traversal,
  closing a symlink escape that could walk outside configured read roots while
  preserving in-root symlink-loop detection.

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

### WS5 — Steering and host orchestration

- Add agent-scoped, in-memory `SteeringHandle` queues for live steers and
  would-stop-only follow-ups, with `OneAtATime`/`All` drain modes, explicit
  idle `run_queued`, rollback-safe inflight requeue through runner and
  finalization errors, deterministic steering-before-strategy precedence, and
  stable steering-before-team-before-background request ordering.
- Add object-safe `TaskStore::mutate` and serialize builtin task mutations with
  SQLite Immediate transactions or the Volatile store lock. Expose typed
  `Runtime::task_board`/`Agent::task_board` façades that reuse the intrinsic
  access checks, status transitions, and DAG validation.
- Add `Agent::run_to_output<T>` using unique owner-scoped terminal tools and
  exact `tool_use_id` transcript-detail extraction. Generated tools are forced
  only for their target agent and removed when the run future completes or is
  dropped.
- Add cloneable `AgentWaitHandle` and owned snapshot, idle, and teammate-reply
  wait futures. `AgentSnapshot::run_generation` prevents a previous terminal
  snapshot from satisfying a wait for the next run.
- Make opaque team-protocol request IDs collision-resistant by retaining the
  timestamp and atomic counter as separate components instead of XORing them.
  This prevents one request from resolving another during cross-second
  interleavings.
- Document the Proposed host-orchestration decision in ADR-0004.

### Compatibility

- Shell validation defaults to `Off`, preserving existing command-execution
  behavior. `RuntimePolicy` only gains private state, so existing constructors
  remain source-compatible.
- Shell `ToolAuthorizationPreview::structured_input` gains an additive
  `validation` object. Consumers comparing that JSON exhaustively must accept
  the new key.
- Results at or below both tool-result limits remain byte-identical. Existing
  results above either new default limit now intentionally become a retained
  head plus notice; the shell stream-capture limit remains separate and still
  applies before this projection boundary.
- `AgentStore` gains a defaulted `allows_disk_artifacts` capability method, so
  existing store implementations continue to compile unchanged.
- File tools still default to the historical batched `files` descriptor and
  behavior. Split tool names and schemas appear only when an embedder selects
  `Split` or `Both`; `files.replace` retains its prior exact-match semantics.
- Recursive batched list/search now reject a descendant symlink that resolves
  outside the runtime read roots. This intentional policy-enforcement fix is
  the sole WS3 default-path behavior change.
- Persisted messages and transcripts from before WS4 deserialize unchanged.
  New optional thinking fields are serde-defaulted and omitted when absent.
- Responses streams without a reasoning item retain their historical plain
  `call_id` tool-use IDs; composite IDs are limited to newly preserved
  reasoning/tool associations.
- `ContentBlock`, `ContentBlockStart`, `ContentBlockDelta`, `AgentEvent`,
  `SessionEvent`, and `ResponsesInputItem` gain public variants. Exhaustive
  matchers must add the new reasoning cases (or a deliberate fallback);
  existing non-exhaustive usage is unchanged.
- `TaskStore` gains a defaulted, object-safe `mutate` method, so existing
  implementations remain source-compatible. Its fallback is load/replace and
  is not atomic; custom stores must override it for concurrent-writer safety.
- `AgentSnapshot` gains the public serde-defaulted, zero-omitted
  `run_generation` field, so old snapshots load unchanged and generation-zero
  snapshots keep the old serialized shape. Exhaustive struct literals must add
  it and exhaustive patterns should use `..`.
- Default agents have empty steering queues and unchanged run behavior.
  `wait_for_teammate_reply` intentionally consumes inbox delivery rather than
  peeking; hosts that previously polled `pending_team_messages` separately can
  continue doing so unchanged.
- Keep the public `time` and `url` requirements semver-compatible so downstream
  workspaces can select compatible releases. Workspace resolver v3 and a fresh
  lockless Rust 1.85 CI job select dependencies compatible with Mentra's MSRV
  instead of imposing exact versions on consumers.

### Downstream migration

- Upgrade `mentra` and `mentra-provider` together. `mentra 0.10.0` depends on
  `mentra-provider 0.4.0`; consumers that name provider-core types directly
  must not retain `mentra-provider 0.3.x` alongside it.
- Machine-readable string tool protocols must account for the new default
  2,000-line / 50-KiB result cap. A single JSON line above 50 KiB is replaced by
  a truncation notice and is no longer parseable JSON. Until such a protocol
  implements its own bounded projection, preserve the old behavior with:

  ```rust
  RuntimePolicy::default()
      .with_max_tool_result_bytes(usize::MAX)
      .with_max_tool_result_lines(usize::MAX)
      .spill_full_tool_output(false)
  ```

- Publish `mentra-provider 0.4.0` before `mentra 0.10.0`. The repository uses
  one `v0.10.0` tag for the combined release.

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
