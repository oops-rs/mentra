# Agentic Core Hardening — Investigation & Design Plan

> Status: Investigation complete (2026-07-16). Designs proposed, not yet decided.
> Scope: mentra core only. The coding agent itself is a separate downstream
> project; features like AGENTS.md loading, a wire protocol/RPC surface, and
> session UX belong there, not here.
> Origin: readiness audit of mentra 0.9.0 measured against `pi` (reference
> architecture per ADR-0001) and codex. Five workstreams below make mentra
> great at the agentic-core gaps that audit found.
>
> Note: file:line references were verified against the tree at the time of
> investigation and will drift as the code changes.

Repository-wide MSRV correction (2026-07-16): fresh dependency resolution
invalidated the assumption that the declared Rust 1.85 floor was enforced by
the existing broad version ranges. `time` could resolve to a release requiring
Rust 1.88, while `url` 2.5.8 selected IDNA/ICU releases requiring Rust 1.86.
Both public crates now pin `time` 0.3.45 and `url` 2.5.2; the latter is the
narrow direct constraint that retains the pre-ICU IDNA path without pinning a
cluster of implementation-detail transitive crates.

## Workstream overview

| # | Workstream | Effort | Risk | ADR needed |
|---|---|---|---|---|
| 1 | Hygiene: dead provider tree + bash_validation wiring | ~1 day | Low | No |
| 2 | Generic tool-output truncation + builtin tool profiles | ~2–3 days | Low | Folded into ADR for WS3 |
| 3 | Model-conventional coding tools (read/edit/write/grep/glob/ls) | ~2 weeks | Low–Med | Yes (ADR-0002 candidate) |
| 4 | Thinking/reasoning preservation across providers | ~2–3 weeks | Med | Yes (ADR-0003 candidate) |
| 5 | Steering queue + host orchestration API | ~3–4 days | Med | Yes (ADR-0004/0005 candidates) |

Recommended sequence: WS1 → WS2 → (WS3 ∥ WS5) → WS4. WS2 is independently
shippable and the highest immediate model-facing value per day of work. WS4 is
the deepest change (touches mentra-provider + core) and can proceed in
parallel once its ADR is settled.

---

## WS1 — Hygiene

### 1a. Delete the orphaned `mentra/src/provider/` subtree

**Confirmed dead: 16 files, 5,924 lines** (earlier estimate of ~1,279 lines
counted only the top level and missed four subdirectories).

- `mentra/src/provider.rs` declares its submodules **inline** (`pub mod openai {`
  at provider.rs:368, plus openrouter:522, anthropic:584, gemini:637,
  ollama:690, lmstudio:760, `pub mod model {` at :57). No file-form
  `mod` statement and no `provider/mod.rs` exists, so nothing under
  `mentra/src/provider/` is ever compiled.
- Orphaning event: `2ab22d1 refactor(core): extract mentra-provider crate`
  swapped file mods for inline modules delegating to the new crate.
  `ollama.rs`/`lmstudio.rs` were born dead (added post-extraction).
- Salvage assessment: nothing worth keeping — every public symbol in the dead
  files is preserved by the live inline modules wrapping `mentra_provider::*`.
- All `crate::provider::model::ModelInfo` references resolve to the inline
  `pub mod model`, not the dead file. Zero references from examples or tests.

Action: `git rm -r mentra/src/provider/` (the directory, not `provider.rs`).

### 1b. Wire `bash_validation` (or stop shipping it unwired)

Current state: `mentra/src/tool/bash_validation.rs` (810 lines, ~30 unit
tests) is **public API** (`mentra::tool::bash_validation::*` via tool.rs:3 +
lib.rs:32) with **zero internal callers**. CHANGELOG 0.7.0's "bash command
validation for safer shell-tool execution" is currently misleading.
`docs/safe-shell-command.md` is an aspirational 5-layer proposal; this module
implements only a crude slice of layers 1–2 (heuristic substring
classification — trivially bypassed via `/bin/rm`, `$(...)`, quoting). It is a
guardrail/UX signal, **not a security boundary**; the boundary remains
RuntimePolicy roots + env_clear + setsid + process-group kill.

Design (default-off, non-breaking, ~100–150 LOC + tests, ~0.5 day):

```rust
pub enum ShellValidationMode { Off, Warn, Enforce } // RuntimePolicy field, default Off
// builder: RuntimePolicy::shell_validation(mode)
```

- **Enforcement hook:** `build_command_request`
  (runtime/handle/execution.rs, after the `authorize_command_execution` block
  at :15-30). `Block` + Enforce → `Err` + emit the existing
  `RuntimeHookEvent::AuthorizationDenied`; `Block` + Warn → emit hook,
  proceed; `Warn` result → hook only.
- **Preview hook:** enrich `shell_authorization_preview` (shell.rs:122)
  `structured_input` with `classify_command` intent + `ValidationResult` so
  `ToolAuthorizer` hosts render the classification in permission prompts.
  Purely additive JSON.
- Map `ValidationResult` → existing `ToolAuthorizationOutcome`
  (Block→Deny, Warn→Prompt, Allow→Allow) so both hooks share semantics.
  This mirrors codex's execpolicy: a three-valued classifier decision
  (`Allow/Prompt/Forbidden`), with a separate policy deciding how `Prompt`
  collapses (codex-rs/core/src/exec_policy.rs:269,326).
- Known impedance: `validate_command` wants a workspace `Path` + `read_only`
  bool; derive `read_only = allowed_write_roots.is_empty()` and pass the first
  working root. Add a policy accessor for these.

Implementation correction (2026-07-16): `allowed_working_roots` and
`allowed_write_roots` are extra roots; `AgentExecutionConfig::base_dir` is an
implicit allowed root even when those vectors are empty. Validation therefore
falls back to `base_dir` when there is no explicit working root. The locked
`allowed_write_roots.is_empty()` rule is intentionally retained, but its
classifier notion of `read_only` is narrower than the file-policy invariant:
default/permissive policies can still write inside `base_dir`, while opt-in
shell validation classifies them as read-only. Correcting that pre-existing
policy-model mismatch is outside WS1 because it would change default file-tool
authorization behavior.

The locked hook reuse also means `RuntimeHookEvent::AuthorizationDenied` can
represent a non-denying shell-validation warning. Implementations distinguish
these classifier signals with the `shell_validation` /
`background_shell_validation` action and the mapped authorization outcome in
the tool preview; consumers must not infer final denial from the event name
alone.

---

## WS2 — Generic tool-output truncation + builtin tool profiles

### Truncation

**Single executor-output choke point confirmed:**
`ToolRuntime::tool_output_block` (tool/orchestrator.rs:348) — every actual
executor result (Ok and Err, parallel lane :655 and exclusive lane :806,
builtin/custom/MCP) passes through it before becoming a provider-visible
`ContentBlock::ToolResult`. Synthetic results for missing tools,
authorization/pre-execution-hook failures, and calls skipped after termination
are constructed separately and do not pass this boundary. Truncating actual
outputs here means transcript, replay, persistence, compaction, and provider
all agree, while `details`/`terminate` remain untouched.

Design:

- Head-truncate to whichever of a byte and line limit is hit first; never emit
  a partial line; append `[truncated: showing N of M lines; full output at
  <path>]`. Defaults matching model expectations (pi parity):
  **2000 lines / 50 KB**.
- `ToolResultContent::Structured` is never cut mid-JSON: if its stringified
  size exceeds budget, spill and replace with a Text pointer.
- Truncate error strings too.
- Spill full output to
  `AgentConfig::compaction.transcript_dir.join("tool-output")`, the existing
  runtime-managed artifact root. `AgentStore::allows_disk_artifacts` defaults
  true and is false for `VolatileRuntimeStore`, so volatile runs still leave no
  durable trace. Disabled or failed spills retain an actionable explanation in
  the notice instead of changing the tool's success/error state.
  Requires making `tool_output_block` an instance method (`&self`) —
  mechanical, both call sites already run on `&self`/`&mut self`.
- Config on `RuntimePolicy` beside `max_output_bytes_per_stream`
  (policy.rs:19,34): `max_tool_result_bytes` (50*1024),
  `max_tool_result_lines` (2000), `spill_full_tool_output` (true).
  This is complementary to the shell **stream** cap, which stays.
- Per-result truncation is race-free under the parallel JoinSet (results land
  by index, order reconstructed at orchestrator.rs:696-701).
- The shell stream cap is upstream: a shell spill preserves the complete
  captured stream, but cannot recover bytes already discarded by
  `max_output_bytes_per_stream`.
- Compatibility correction: the new 50-KB/2,000-line defaults intentionally
  change existing oversized outputs. The byte-identical guarantee applies to
  under-limit results, not results that now require truncation.

### Builtin tool profiles

Composition is ~80% present: agent-level `ToolProfile { allowed, hidden }`
(agent/config.rs:136, enforced at agent.rs:472) and same-name re-register
replaces (tool.rs:49-55). Missing piece:

```rust
pub enum FileToolProfile { Batched /* default */, Split, Both }
// RuntimeBuilder::with_file_tools(profile), branching in register_builtin_tools (tool.rs:94)
```

Default `Batched` keeps existing embedders byte-identical.

Implementation sequencing correction (2026-07-16): builtin registration is
eager in `RuntimeHandle::new(true)`, and `Split`/`Both` have no truthful meaning
until the WS3 executors exist. `FileToolProfile` therefore lands together with
WS3, where the builder can reconfigure the eager registry without exposing
no-op variants. WS2 ships the independently useful truncation seam first.

---

## WS3 — Model-conventional coding tools

### Why the split is cheap

Every direct `WorkspaceEditor` op already did its own path resolution,
`..`-escape normalization, read/write authorization against policy roots, and
overlay-aware existence checks. The **only** batch-entangled machinery is the
staged overlay + atomic commit — and for single-op tools it degrades gracefully:
reads leave the overlay empty (commit is a no-op), single writes get temp-file
+ atomic rename per file. The only lost property is cross-file
transactional rollback, which no single-file conventional tool needs. The
batched `files` tool stays for hosts that want transactions; both surfaces
share one engine.

Implementation correction (2026-07-16): the investigation's authorization
claim was incomplete for recursive operations. `list` and `search` authorized
their root once, then followed descendant symlinks without reauthorizing each
target. A symlink beneath an allowed root could therefore make a recursive walk
read outside the policy roots. WS3 first moved list/search/glob onto one walker
that reauthorizes every descendant before inspecting or following it. Escaping
descendants now reject the operation and emit the existing authorization hook;
in-root symlink loops remain bounded by canonical visited-directory tracking.
This is an intentional policy-enforcement correction to the otherwise
byte-identical default batched profile.

### The tool set

| Tool | Params (snake_case + camelCase aliases) | Category | Backing op |
|---|---|---|---|
| `read` | `path\|file_path, offset?≥1, limit?` | ReadOnlyParallel | `read` (operations.rs:49) |
| `ls` | `path?, depth?, limit?` | ReadOnlyParallel | `list` (:74) |
| `grep` | `pattern, path?, glob?, ignore_case?, literal?, context?, multiline?, limit?` | ReadOnlyParallel | `search` (:110), extended |
| `glob` | `pattern, path?, limit?` | ReadOnlyParallel | **new** op: list-walk + `glob_match` (dep already in workspace) |
| `write` | `path\|file_path, content` | ExclusiveLocalMutation | `create`/`set` (:145/:161) |
| `edit` | `path\|file_path, edits:[{old_string\|oldText, new_string\|newText}], replace_all?` | ExclusiveLocalMutation | `replace` (:176), hardened |

`move`/`delete` stay in batched `files`/shell initially. Read-only tools set
their category statically (the dynamic `file_execution_category` in
`files/input.rs` exists only because the batched tool mixes read+write).
Implementation: make the nine op methods `pub(crate)`, add thin executors over
the same engine, and select their registration through
`FileToolProfile::{Batched, Split, Both}`. Eager builtin registration is
reconfigured immediately by `RuntimeBuilder::with_file_tools`; `Batched`
remains the default.

### Edit hardening (port of pi's edit-diff playbook)

mentra's `replace` today is raw byte `match_indices`/`replacen` — CRLF, BOM,
and smart-quote mismatches silently fail. Port from
`pi/packages/coding-agent/src/core/tools/edit.ts` + `edit-diff.ts`:

1. Strip BOM, detect line endings, normalize to LF before matching; restore
   both on write.
2. Uniqueness, overlap (across sorted multi-edits), and no-op guards, matched
   against the *original* content.
3. Fuzzy fallback: exact match first, then NFKC + trailing-whitespace strip +
   smart-quote/Unicode-dash normalization on both sides, overlaying only
   changed lines back onto original bytes.
4. Input normalization shims (pi `prepareArguments`): `edits` sent as a JSON
   string → parse; legacy top-level old/new → fold into `edits[]`.
5. Result content = short `"Replaced N block(s) in <path>"`; the display diff,
   unified patch, and `first_changed_line` ride in **`ToolOutput.details`** —
   confirmed the right carrier (opaque host metadata, survives transcript +
   compaction per ADR-0001 §3/§4 and the M5 guarantee, never projected to the
   provider). Needs a diff crate (`similar` is idiomatic; none present today).

Compatibility correction (2026-07-16): these semantics belong to the new
opt-in `edit` tool. The default batched `files.replace` path retains its prior
exact-match, expected-count, and first-match behavior byte-for-byte. Routing the
legacy operation through fuzzy/uniqueness guards would violate the additive
default contract even though ordinary tests still passed. `similar` 2.7.0
(MSRV 1.60) and `unicode-normalization` 0.1.25 (MSRV 1.36) both remain below
mentra's Rust 1.85 MSRV.

### Grep gaps to close

The split `grep` path adds multiline matching and a 500-character rendered-line
cap. The cap is an adapter-selected `SearchOptions` value rather than a new
default for legacy `files.search`, whose long-line output remains byte-identical
for existing embedders.

### Effort

Thin executors ~1d; glob op ~0.5d; grep multiline + line cap S; edit hardening
~2–3d; aliases/normalizer S. Test oracles: existing `files` tests for parity;
key new asserts include "details absent from provider projection" (extend
agent/tests/tool_output.rs) and sandbox/policy parity per split tool.

---

## WS4 — Thinking/reasoning preservation

### Corrected baseline (important)

**No provider round-trips reasoning today — including OpenAI Responses.**
Responses streams reasoning *deltas* and leans on server-side state
(`previous_response_id`); it does not capture the reasoning output item
(`ResponsesOutputItem` has no reasoning arm → `Unsupported`, sse.rs:448-495),
has no reasoning input item to replay, and **loses reasoning on the Hybrid
replay fallback** (session.rs:415-423). There is no neutral Thinking content
block anywhere (`ContentBlock`, mentra-provider/src/model.rs:279-306). This is
an add-a-first-class-content-type project, not a decoder fix.

Three drop points: (1) provider decode — Anthropic thinking →
`#[serde(other)] Unsupported` (anthropic/stream_model.rs:36-48,66,77-86),
Responses reasoning item → Unsupported, Gemini thought not modeled
(gemini/sse.rs:366-372); (2) stream→Response collapse ignores the three
reasoning events (response.rs:218-220); (3) runner accumulation ignores them
(agent/pending.rs:39-43), so committed Messages/transcript/host events never
see reasoning.

### Design

New neutral type (additive, externally tagged — old persisted transcripts
load unchanged):

```rust
ContentBlock::Thinking {
    thinking: String,                      // "" for pure-redacted
    signature: Option<String>,             // Anthropic sig / Gemini thoughtSignature
    encrypted_content: Option<String>,     // OpenAI
    id: Option<String>,                    // OpenAI reasoning item id
    provenance: Option<ReasoningProvenance>, // { provider, model, format }
    redacted: bool,
}
enum ReasoningFormat { AnthropicSigned, OpenAiEncrypted, GeminiThought }
```

Plus `ContentBlockStart::Thinking` / `ContentBlockDelta::ThinkingText` /
`::ThinkingSignature` in stream.rs so the existing index-keyed block machinery
(response builder BTreeMap response.rs:180; pending accumulator pending.rs:17)
accumulates it. Anthropic emits thinking at index 0, so thinking-first
ordering on replay falls out of the existing order-preserving serialization
(model.rs:276) — only types are missing, not plumbing.

Correctness rules (pi-verified):

1. **Replay gate = exact provider AND model match** via `provenance`
   (pi gates on `isSameProviderAndModel`). Never send one provider's signature
   to another — or to a different model of the same provider.
2. **Fallback = downgrade-to-text, never skip or error.** Unreplayable
   thinking (cross-provider/model, missing/empty/invalid signature) becomes a
   plain Text block. Critical edge case: an **aborted stream yields a thinking
   block with an empty signature**; replaying it to Anthropic is rejected, so
   it must downgrade. Persisted transcripts then never hard-fail on replay.
3. **Host events carry thinking text only; the signature attaches at block
   close** (Anthropic sends `signature_delta` silently; Gemini may send the
   signature only on the first delta — retain, don't null on later deltas).
   New events: `AgentEvent::ReasoningDelta` (emitted from pending.rs) →
   `SessionEvent::AssistantReasoningDelta`, mirroring AssistantTokenDelta via
   session/mapping.rs.
4. **OpenAI reasoning-item ↔ tool-call pairing must survive replay** (pi
   stores the reasoning item JSON keyed by item id and composite-encodes
   `call_id|item_id` so they re-link; cross-model, the `fc_...` id is nulled).
   Azure quirk: `encrypted_content` can be absent on item.done and must be
   backfilled from `response.completed`.

Per-provider work:

- **Anthropic (highest value — required for Claude tool-loop correctness):**
  capture `Thinking`/`RedactedThinking` stream blocks + `ThinkingDelta`/
  `SignatureDelta`; replay arms in `From<&ContentBlock>` (model.rs:315-356)
  keeping thinking first; also the non-stream `TryFrom` arms (:358-381).
- **Responses:** capture `ResponsesOutputItem::Reasoning { id,
  encrypted_content, summary }`; replay as a reasoning `ResponsesInputItem`
  (belt-and-suspenders with `previous_response_id`); request
  `include:["reasoning.encrypted_content"]` when reasoning is on
  (settable at model.rs:84-85, currently unconsumed).
- **Gemini:** add `thought`/`thoughtSignature` to response `GeminiPart`,
  branch thought parts to Thinking (not Text); request `includeThoughts`.
  Full Gemini fidelity eventually needs signatures on **ToolUse** (Google's
  `thoughtSignature` rides on any part) — explicitly **phase 2**; document the
  limitation now.

Compaction/persistence: safe by construction — preserved tail keeps
TranscriptItems verbatim (thinking survives the in-flight tool loop, guarded
by `required_tail_start_for_continuation`, memory/compaction.rs:67-89);
summarizer input uses `item.text()` = Text only, so thinking is naturally
excluded (also strip it from the full-transcript JSON handed to the local
summarizer, compaction.rs:449-450). Serde: all new fields
`#[serde(default, skip_serializing_if)]`; the `deserialize_transcript` compat
shim (journal/state.rs:33-48) is the precedent. Keep `signature` an opaque
`Option<String>`; if it ever becomes structured, version it (`{v:1,...}`) with
dual-read of the bare form.

Phasing: **P1** neutral type + builders + events + Anthropic capture/replay +
downgrade-to-text guard. **P2** Responses item capture/replay + id pairing.
**P3** Gemini thoughts + ToolUse signature carriage.

---

## WS5 — Steering queue + host orchestration API

### Constraint that shapes both designs

`Agent::run(&mut self)` holds the borrow for the whole run
(lifecycle.rs:34), so live-run interaction goes through `Arc`-shared handles
obtained before the run — the pattern `subscribe_events()`/`watch_snapshot()`
already use.

### 5a. Steering / follow-up queue

Today's intake: the runner's first act each round is `inject_team_inbox()` +
`inject_background_notifications()` (runner.rs:286-287), with an
inflight-tracking + requeue-on-error pattern (lifecycle.rs:63-64 on Ok,
:82-83 on Err) that makes injected messages rollback-safe. That is the
pattern to copy. pi's semantics (the design target): `steer` is polled after
every turn end and can *prevent* the loop from stopping; `followUp` is polled
only when the loop would otherwise exit; queue modes `all` /
`one-at-a-time` (default).

Design: new `agent/steering.rs` (~150 lines) —

```rust
pub enum QueueMode { All, OneAtATime }          // default OneAtATime
#[derive(Clone)] pub struct SteeringHandle { /* Arc<Mutex<SteeringQueues>> */ }
impl SteeringHandle {
    pub fn steer(&self, content: ...);          // next boundary while running
    pub fn follow_up(&self, content: ...);      // only when run would stop
    pub fn clear_steer(&self); pub fn clear_follow_up(&self);
    pub fn has_pending(&self) -> bool;
    pub fn set_steer_mode(&self, QueueMode); pub fn set_follow_up_mode(&self, QueueMode);
}
impl Agent {
    pub fn steering_handle(&self) -> SteeringHandle;  // clone Arc BEFORE run()
    pub fn steer(...); pub fn follow_up(...);         // idle convenience
    pub async fn run_queued(&mut self, options: RunOptions) -> ...; // idle steer becomes next turn
}
```

- Drain at the two existing boundaries via `inject_round_context`
  (runner.rs:276-283): at **AssistantMessageCommitted** (no-tool-calls branch,
  runner.rs:139) drain steer, else follow_up — either yields → inject +
  `continue` (a steer prevents stopping, pi parity); at
  **ToolResultsCommitted** (runner.rs:203) drain steer only.
- **Orthogonal to `RoundStrategy`, with fixed precedence:** `RunOptions` has a
  single `round_strategy` slot (run.rs:51), so steering must not consume it.
  If the steering queue yields at a boundary, inject and skip the user
  strategy this round (it sees the next boundary). No round double-injects.
  (A `SteeringStrategy: RoundStrategy` draining a shared VecDeque is a
  zero-runner-change spike, but it steals the slot and can't requeue on
  rollback — prototype only.)
- Rollback safety: `inflight_steer`/`inflight_follow_up` on Agent mirroring
  `inflight_team_messages` (agent.rs:80); clear on Ok, requeue on Err.
- Queue is **Agent-scoped** (not RunOptions-scoped): preserves ADR-0001
  per-run isolation across a pooled runtime while intentionally surviving
  between one agent's runs.
- Document a stable drain order relative to team-inbox injection.

Effort ~1–1.5 days. Tests: steer visible in next provider request (scripted
provider); follow-up only at would-stop; QueueMode variants; requeue on error
then re-inject on resume; precedence over user RoundStrategy; idle
`run_queued`; cross-agent isolation.

### 5b. Public task board

All mutation logic + DAG validation already exists sealed behind
`RuntimeHandle::execute_task_mutation` (runtime/handle/execution.rs:233 →
task::execute_with_store, runtime/task.rs:41; cycle detection graph.rs:89).
Expose a typed façade that **wraps** (never reimplements) that path:

```rust
impl Runtime { pub fn task_board(&self, namespace: impl AsRef<Path>) -> TaskBoard; } // Lead
impl Agent   { pub fn task_board(&self) -> TaskBoard; }  // inherits agent's access
impl TaskBoard {
    pub fn create(&self, spec: NewTask) -> Result<TaskItem, TaskBoardError>;
    pub fn get(&self, id: u64) -> ...; pub fn list(&self) -> ...;
    pub fn update(&self, id: u64, patch: TaskPatch) -> ...;
    pub fn claim(&self, id: Option<u64>, owner: &str) -> ...;
    pub fn add_dependency(&self, blocker: u64, dependent: u64) -> ...;
    pub fn remove_dependency(&self, blocker: u64, dependent: u64) -> ...;
}
```

Access invariants: host = `TaskAccess::Lead`; agent-scoped boards keep
teammate restrictions (own-tasks-only, no dependency edits) — never let a host
silently pose as a teammate.

**Atomicity gap (pre-existing, widened by a host writer):**
`execute_with_store` is load → mutate-in-memory → replace as two separate
store transactions (task.rs:51-71), and lead + teammates already share one
namespace — last-writer-wins races exist today. Fix: transactional
`TaskStore::mutate(namespace, f)` under a single immediate transaction
(sqlite already uses `TransactionBehavior::Immediate` per call, store.rs:1155);
route `execute_with_store` + façade through it. Touches all three store
impls. Confirm no higher-level lock exists before building (none was found).

### 5c. Typed run results

Pieces already shipped in 0.9.0: `ToolOutput { details, terminate }`,
`terminal()` descriptor coercion to the exclusive lane, details keyed by
`tool_use_id` on the transcript (orchestrator.rs:126-129, runner.rs:191-195),
and public readback via `Agent::transcript()` + `TranscriptItem::detail(id)`
(transcript.rs:160-166). Helper to compose them:

```rust
pub struct TerminalOutputSpec { pub tool_name: String, pub description: String, pub schema: Value }
pub struct FinalOutput<T> { pub value: T, pub message: Message }
impl Agent {
    pub async fn run_to_output<T: DeserializeOwned>(
        &mut self, content: ..., options: RunOptions, spec: TerminalOutputSpec,
    ) -> Result<FinalOutput<T>, RuntimeError>;
}
```

Terminal tool returns `ToolOutput::structured(input).with_details(input)
.terminating()`; after Ok, deserialize the last item's `detail(tool_use_id)`.
Optionally force via `tool_choice::Tool`. Wrinkles: tools are runtime-global
(runtime.rs:82) — use unique names or per-agent `tool_profile` gating; key
extraction by `tool_use_id`. Provider-native request-level structured output
(`response_format`) does **not** exist today (the Request built at
runner.rs:304-314 has no such field) — defer as a provider-layer enhancement.

### 5d. Wait/notify conveniences

Push channels already exist (`subscribe_events`, `watch_snapshot`; teammate
replies already bump `pending_team_messages` via snapshot push,
runtime/handle/agents.rs:33-49). Add thin wrappers:

```rust
impl Agent {
    pub async fn wait_for_snapshot(&self, pred: impl Fn(&AgentSnapshot) -> bool) -> AgentSnapshot;
    pub async fn wait_until_idle(&self) -> AgentSnapshot;   // Finished/Failed/Interrupted
    pub async fn wait_for_teammate_reply(&self) -> Result<Vec<TeamMessage>, RuntimeError>;
}
```

### Host-scripted orchestration, end to end

Assign via `runtime.task_board(ns).create(...)` / `send_team_message`; drive
via `tokio::spawn(agent.run(...))` holding `steering_handle()` +
`watch_snapshot()`; collect via `run_to_output::<T>()` or
`wait_for_teammate_reply()`. No LLM routes any of it.

---

## Open decisions

1. **Steering persistence:** in-memory with requeue-on-error (recommended) vs
   SQLite-persisted like team-inbox.
2. **Idle steer:** explicit `run_queued()` (recommended) vs auto-starting a run.
3. **Boundary drain order:** steering-queue > user RoundStrategy (recommended);
   fix and document order relative to team-inbox.
4. **Task-board atomicity:** add `TaskStore::mutate` seam (recommended — the
   race pre-exists) vs document last-writer-wins.
5. **Thinking scope:** signatures on `ToolUse` blocks (full Gemini fidelity)
   now or phase 3 (recommended: phase 3, document the limitation).
6. **ShellValidationMode default:** Off (recommended) — permissive()/
   workspace_bounded()/read_only() behavior unchanged until opt-in.

## Source of findings

Produced by four parallel read-only investigations (2026-07-16) over this
repo, with pi (`/…/WeNext/ai/pi`) and codex (`/…/WeNext/ai/codex`) as
reference implementations. Corrections to the original audit folded in:
dead-tree size is 5,924 lines (not ~1,279); no provider round-trips reasoning
today (not even Responses); bash_validation is public API, not just unused
internals.
