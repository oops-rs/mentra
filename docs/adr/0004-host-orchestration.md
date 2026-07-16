# ADR-0004 — Add agent-scoped host orchestration primitives

> Status: Proposed
> Created: 2026-07-16
> Companion decision: [ADR-0001 — Evolve mentra into a pi-shaped pluggable
> agent core](0001-pluggable-agent-core.md).

## Context

ADR-0001 keeps Mentra as the sole owner of the inner think/tool/observe loop.
Hosts therefore need generic ways to influence an active agent, coordinate its
task state, collect a typed terminal value, and wait for lifecycle changes
without borrowing the agent around that loop.

The existing pieces are close but incomplete. `Agent::run(&mut self)` holds the
mutable borrow for the entire run. `RunOptions::round_strategy` has only one
host-owned slot and is scoped to one invocation. Task mutations validate their
DAG correctly but previously persisted via a non-atomic load/replace pair.
Structured terminating tool output already preserves opaque details, while the
tool registry is runtime-global. Snapshot and team-inbox notifications already
use watch channels, but a future borrowing `&Agent` cannot be polled alongside
`run(&mut Agent)`.

These constraints require handles and store seams that preserve the one-loop
architecture, agent isolation, rollback safety, and existing default behavior.

## Decision

1. **Steering is an in-memory, agent-scoped queue.** Every agent owns an
   `Arc`-shared `SteeringHandle` with independent steer and follow-up queues.
   `QueueMode::OneAtATime` is the default; `All` drains all currently queued
   entries into one round. Idle entries start work only through explicit
   `Agent::run_queued`; enqueueing never auto-starts a run.

2. **Boundary precedence and delivery order are fixed.** A terminal tool or
   `end_turn` stops before queue inspection. At either non-terminal boundary a
   steer wins over the user `RoundStrategy`, is injected once, and skips that
   strategy for the boundary. A follow-up is inspected only at the tool-free
   assistant boundary where the run would otherwise stop. The stable order in
   the next provider request is **steering → team inbox → background**. Limits
   and graceful stops are checked before drain, so no queue entry is consumed
   without an eligible next request.

3. **Queue delivery is rollback-safe.** Drained entries move into agent-local
   inflight vectors. Successful runs clear them; failed runs prepend them to
   their original queue, ahead of entries added later. Queues are deliberately
   not persisted and never cross agent identities on a shared runtime.

4. **Task mutations gain an object-safe transactional seam.** `TaskStore::mutate`
   accepts an erased callback so `dyn TaskStore` remains valid. Its default
   load/replace implementation preserves source compatibility but cannot promise
   atomicity. SQLite overrides it with an Immediate transaction, Volatile uses
   clone-then-install under one mutex, and Hybrid delegates. The typed
   `TaskBoard` façade routes every operation through the existing intrinsic task
   executor, preserving one implementation of access and DAG validation.
   `Runtime::task_board` has Lead access with an explicit claimant;
   `Agent::task_board` retains that agent's actual Lead or Teammate access.

5. **Typed output is an agent-scoped terminal tool protocol.**
   `Agent::run_to_output<T>` registers a unique, owner-scoped terminal tool,
   forces only that tool for the target run, and removes it through an RAII
   guard. The tool returns its input as structured content and opaque details,
   then terminates. Extraction requires a newly emitted matching tool name, the
   exact `tool_use_id`, and details on the last transcript item. The helper
   accepts the underlying committed `EmptyAssistantResponse` only after those
   checks. Provider-native `response_format` remains out of scope.

6. **Waits are owned and generation-aware.** `AgentWaitHandle` clones the
   existing watch receiver and returns owned futures, allowing waits to coexist
   with a mutable run. `AgentSnapshot::run_generation` distinguishes the next
   run from a stale terminal snapshot. Teammate-reply waiting consumes the
   inbox: its read transitions pending delivery to inflight and must not race the
   agent loop's own inbox read.

## Consequences

- Hosts can steer active work, enqueue a would-stop follow-up, collect typed
  results, and coordinate task status while Mentra remains the only inner loop.
- Existing agents have empty queues and unchanged behavior. Existing custom
  `TaskStore` implementations compile through the default `mutate`, although
  they must override it to gain concurrency guarantees.
- Runtime-global terminal registration cannot leak execution or results across
  agents: visibility and execution are owner-checked, names are unique, result
  lookup is bounded to new transcript items, and cleanup runs when the future is
  completed or dropped.
- TaskBoard reads bypass the agent's cached task snapshot. Callers that mutate
  through the façade should expect the cache to refresh at the next normal
  refresh/run boundary.
- The existing full-board failed-run restore remains a distinct race: it can
  overwrite a concurrent successful mutation that occurred after capture.
- Host reply waits are destructive delivery operations, not passive peeks; the
  host must route returned content explicitly if a model should see it.

## Rejected

- **Attach steering to `RunOptions` or implement it as a `RoundStrategy`.**
  Rejected because it would consume the host's strategy slot, lose agent-scoped
  continuity, and make rollback requeue awkward.
- **Persist steering in SQLite or auto-start idle agents.** Rejected because the
  required semantics are process-local and host-controlled; persistence and
  implicit execution would add lifecycle policy not justified by the seam.
- **Reimplement task mutations in `TaskBoard`.** Rejected because it would split
  access checks, status propagation, and cycle validation across two paths.
- **Make a generic `TaskStore::mutate<F, T>`.** Rejected because a generic trait
  method would make `TaskStore` non-object-safe. The erased callback keeps the
  existing `dyn TaskStore` architecture.
- **Treat `EmptyAssistantResponse` as unconditional typed success.** Rejected
  because an unrelated empty response or stale detail could be misreported as a
  valid terminal value.
- **Use provider `response_format` for typed output.** Rejected for this change:
  the neutral request has no such field, while a terminal tool already provides
  a provider-independent contract.
- **Return waits that borrow `&Agent`.** Rejected because such futures cannot be
  polled while `run(&mut Agent)` owns the mutable borrow.
