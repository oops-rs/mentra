# ADR-0001 — Evolve mentra into a pi-shaped pluggable agent core

> Status: Draft
> Created: 2026-07-11
> Companion decision (nous):
> [ADR-0052 — Evolve the agent harness through typed finish and per-run mentra
> seams](../../../nous/docs/adr/0052-agent-harness-evolution.md) and proposal
> [0050 — Agent harness redesign](../../../nous/docs/proposals/0050-agent-harness-redesign.md).
> Reference architecture: `pi` (architectural reference only — no language
> change; mentra stays Rust).

This is the first ADR recorded for mentra. mentra adopts the nous ADR format:
one decision per file, four-digit monotonic numbering under `docs/adr/`, a status
line, and the `Context` / `Decision` / `Consequences` / `Rejected` sections.

## Context

mentra owns the inner think/tool/observe loop. That ownership is deliberate and
must not move: hosts embed mentra rather than reimplement a loop around it. The
loop lives in `mentra/src/agent/runner.rs` as `TurnRunner<'a>`, driven from
`Agent::run` in `mentra/src/agent/lifecycle.rs`.

Today that loop hardcodes the policy it applies at every seam a host would want
to influence:

- **Turn lifecycle.** `TurnRunner::run` decides, inside the loop body, that a
  tool-free assistant message ends the run (`runner.rs`, the
  `tool_calls.is_empty()` branch returns `Ok(())`), and that after committed tool
  results the loop simply continues unless `execution.end_turn` is set. There is
  no per-run hook invoked at those two boundaries. A host that needs to inspect
  what just happened, inject corrective context, or change the next round's model
  can only do so by driving many short `Agent::run` calls from outside and
  stitching the transcript back together.
- **Tool dispatch.** A custom tool implements `ToolExecutor` / `ExecutableTool`
  from `mentra/src/tool/model.rs` and returns `ToolResult` — the alias
  `Result<String, String>` at `mentra/src/tool/model.rs:414`. A tool therefore
  cannot return structured, machine-readable output, cannot attach durable
  application metadata that survives replay, and cannot ask the loop to stop as
  the value of its own execution. The only termination signal available to a tool
  is the out-of-band `ToolContext::request_idle` (`mentra/src/tool/model.rs`),
  which the orchestrator observes after the fact as
  `agent.take_idle_requested()` in `mentra/src/tool/orchestrator.rs:639`. The
  provider layer already models richer content:
  `ToolResultContent::{Text, Structured}` is already defined in
  `mentra-provider/src/model.rs:104`. The gap is between the custom-tool surface
  and that provider content, not in the provider itself.
- **Judgment / stop policy.** Stopping is expressed only through
  `RunOptions.stop` (a graceful `CancellationToken`) and the budget checks
  `check_limits` / `model_budget` in `mentra/src/runtime/control/run.rs`. There
  is no seam where a host expresses "continue, but steer" or "this answer is not
  yet valid, keep going" as run-scoped policy. Because there is no such seam,
  hosts fold judgment into external orchestration.
- **Memory.** `CompactionEngine` (`mentra/src/compaction.rs`) and `RuntimeStore`
  (the composition trait over `AgentStore`, `RunStore`, `TaskStore`, and the
  other seams in `mentra/src/runtime/store.rs`) are already injectable, and
  memory recall runs through `Runtime::memory_engine`. But `StandardCompactionEngine`
  does not preserve host metadata across a compaction, and there is no run profile
  that suppresses every persistence class at once for a genuinely ephemeral run.

The consequence is an orchestration tax. A host such as nous pays it by
implementing multi-run steering, forced synthesis, fan-out, retries, and context
guards around a loop it cannot reach into. The companion nous decision (ADR-0052)
records the same boundary from the host side and asks mentra for the minimum
generic seams that let a host inject policy without owning the loop.

`pi` is the reference architecture for the target shape: a small core loop that
exposes named extension points a host plugs, rather than a fixed loop a host
works around. `pi` is a reference only. This decision changes mentra's shape, not
its language; mentra remains Rust and keeps its current provider, runtime, and
tool surfaces.

## Decision

mentra evolves from a fixed loop into a pluggable core by adding a small set of
**additive, generic, per-run** extension points. Every seam defaults to today's
behavior, so existing embedders compile and behave unchanged. No seam encodes a
host-specific concept (nothing about citations, evidence, or answer confidence
enters mentra).

1. **mentra remains the sole inner loop.** These seams let a host inject policy
   into `TurnRunner`; they do not invite a second think/tool/observe loop above
   or below it. This preserves the ADR-0003 boundary that nous depends on.

2. **Per-run round strategy (turn-lifecycle + judgment/stop seam).** Introduce an
   async strategy owned by a single `Agent::run` invocation and carried on
   `RunOptions` (`mentra/src/runtime/control/run.rs`), never on a shared
   `Runtime`. `TurnRunner::run` invokes it at the two existing round boundaries:
   after a tool round's results are committed (after the `execute_calls` /
   `append` block in `runner.rs`), and after a tool-free assistant message is
   committed but before the runner returns (the current `tool_calls.is_empty()`
   branch). The strategy may continue, inject corrective context into the next
   request, switch the next round's model or reasoning settings, or request a
   normal stop. A stop request ends the run gracefully exactly as `stop_requested`
   does today; it does not by itself assert that the run produced a valid answer —
   answer validity is the host's judgment, expressed through the strategy. Because
   the strategy is bound to one run, one run's policy state cannot leak through a
   pooled runtime into another run. Its `None` default is exactly current
   behavior.

3. **Structured tool output with termination (tool-dispatch seam).** Add an
   additive output path alongside `ToolResult`, equivalent to:

   ```text
   ToolOutput {
       content: ToolResultContent,   // provider-visible projection
       details: Option<serde_json::Value>,  // durable host metadata
       terminate: bool,              // loop-control signal
   }
   ```

   `content` reuses the existing `ToolResultContent::{Text, Structured}` from
   `mentra-provider`, so no new provider representation is required. Existing
   `ToolExecutor::execute` / `execute_mut` implementations returning
   `Result<String, String>` adapt through a bridge that maps `Ok(s)` to
   `ToolOutput { content: Text(s), details: None, terminate: false }`. Registration
   through `ToolRegistry::register_tool` (`mentra/src/tool.rs`) is unchanged for
   string tools. A tool that sets `terminate: true` ends the run as the value of
   its own execution — a first-class replacement for the out-of-band
   `request_idle` / `take_idle_requested` signal. A terminating tool must run in
   an exclusive `ToolExecutionCategory` (`mentra/src/tool/descriptor.rs`, where
   `allows_parallel` is already the parallel-eligibility predicate) so it cannot
   race parallel retrieval calls scheduled in the same batch by the orchestrator.

4. **Generic transcript metadata and one projection boundary (memory seam,
   part 1).** The `details` JSON on `ToolOutput` survives the local transcript,
   replay, and compaction path as opaque host metadata. mentra never interprets
   it. Provider projection stays centralized at the single existing boundary that
   already turns internal content into `ToolResultContent`, so a host can recover
   its own durable metadata after a round without teaching mentra any host type.

5. **Volatile run profile (memory seam, part 2).** Provide an in-memory
   store/profile that satisfies the `RuntimeStore` composition
   (`mentra/src/runtime/store.rs`) while disabling every persistence class an
   ephemeral run does not need: agent snapshots, pending-turn/token persistence,
   leases, team and task writes, memory ingest, and transcript files. Suppressing
   only streamed-token deltas is insufficient; the profile must make a run leave
   no durable trace. This is additive: the default composed `RuntimeStore` is
   unchanged.

6. **Evidence-preserving compaction (memory seam, part 3).** `CompactionEngine`
   stays injectable, but the target adds a compaction contract (or a standard
   engine variant) that cannot silently discard the `details` metadata attached by
   seam 3. `CompactionOutcome` already reports `preserved_items` /
   `replaced_items` (`mentra/src/compaction.rs`); the added guarantee is that
   host metadata carried on preserved items is not dropped. A host cannot enable
   model compaction on an evidence-bearing run until this holds.

7. **Shared aggregate budget semantics (judgment/stop seam, budgets).** Budgets
   in `RunOptions` (`model_budget`, `retry_budget`, and `check_limits`) become the
   run's aggregate safety bound. Token limits are evaluated against reported usage
   at round boundaries — the same boundary where `TurnRunner` already emits
   `AgentEvent::UsageReport` — and are disclosed as **soft** bounds unless
   pre-request estimation plus output limiting can prove a hard ceiling. Transport
   retries (the `attempt` counter in `stream_turn`) and logical model rounds are
   kept honestly distinct in what the destination reports — noting that today's
   `model_requests` counter increments per provider request *including* transient
   retries (`runner.rs`, inside the retry loop), so it is a request counter, not a
   pure round counter; the destination exposes the distinction without silently
   changing existing `model_budget` semantics. Child work
   spawned during a run shares the parent's cancellation, deadline, and budget.
   Monetary expense requires an injected, versioned price source and is otherwise
   unsupported; a soft round-boundary bound is never relabeled as a hard cap.

8. **Additive by construction.** Every seam ships with a default equal to
   current behavior. Request-transform and result-rewrite ergonomics are not part
   of this decision; they are added only if a future host design proves them
   necessary, not by assertion.

9. **Released boundary for hosts.** A host integrates against a tagged mentra
   release carrying these seams, not an undocumented local path patch. This
   mirrors nous ADR-0052's requirement that upstream-dependent integration waits
   for a companion mentra decision and a tagged release.

## Consequences

- A host expresses per-round policy (steering, corrective injection, model
  switching, stop) through one run-scoped strategy instead of many external
  `Agent::run` calls, so it stops paying the orchestration tax without mentra
  learning any host concept.
- A custom tool can be a first-class terminal action: it returns structured
  content, attaches durable metadata, and ends the run as its own value. This is
  what lets nous make a typed `finish_investigation` tool the successful terminal
  path (nous ADR-0052) while mentra stays generic.
- Existing string tools keep working unchanged; the structured path is strictly
  additive, so no current `ToolExecutor` implementor breaks.
- Ephemeral runs can leave no durable trace through the volatile profile, and
  metadata-bearing runs can compact without losing host metadata. Persistence and
  compaction remain the default for long-lived agents.
- Round-boundary token accounting is honest about being a soft bound. mentra does
  not claim a hard token or monetary ceiling it cannot enforce.
- Because round strategy and structured tool output are per-run, one run's policy
  and termination state cannot cross into another run through a pooled runtime.
  This removes one class of cross-run interference at the framework level; it does
  not by itself prove that a reused provider session (for example a Responses
  WebSocket) is isolated — that remains a separate, host-verified property.
- The loop boundary is preserved: hosts gain reach into the loop, not a license to
  replace it.

## Rejected

- **Change `ToolResult` in place** to return structured output or a terminate
  flag. Rejected: it breaks every existing `Ok(String)` / `Err(String)` tool. The
  structured path must be additive alongside `ToolResult`.
- **Keep termination as `request_idle` only.** Rejected as the terminal contract:
  an out-of-band idle flag observed after execution cannot carry structured
  result content, and a host cannot distinguish "the model idled" from "a terminal
  action returned a valid result." The `terminate` flag on `ToolOutput` makes the
  signal first-class while `request_idle` remains for its current uses.
- **Attach round strategy to `Runtime` instead of `RunOptions`.** Rejected:
  runtime-scoped policy would let one run's steering and stop state leak into
  another run sharing the runtime.
- **Teach mentra about host types** (citations, evidence, confidence) so metadata
  survives natively. Rejected: metadata stays opaque `serde_json::Value` on
  `ToolOutput` behind one projection boundary, keeping mentra generic.
- **Add request-transform and result-rewrite seams now.** Deferred: not required
  by the current host design, and added only against a proven need rather than by
  assertion.
- **Call a round-boundary usage limit a hard cap.** Rejected: without pre-request
  estimation and output limiting, a token limit is a soft bound, and expense needs
  a versioned price source. Naming it hard would be dishonest.
- **Let a host build its own loop over mentra's lower layers.** Rejected: it
  breaks the ADR-0003 boundary that both repos depend on. The whole point of these
  seams is to make an external loop unnecessary.
