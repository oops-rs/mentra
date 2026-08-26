# ADR-0005 — Make request-only tool-result elision observable

> Status: Accepted (2026-08-26)
> Created: 2026-08-26

## Context

`CompactionConfig::keep_recent_tool_results` controls a request-only projection.
For a finite value `N`, Mentra clones the active provider-neutral history,
preserves the newest `N` tool results, and considers every older result in
chronological order. A considered payload larger than 100 bytes is replaced by
`[Previous: used <tool>]`. The canonical transcript is not mutated.

This policy is deliberately disabled by default with `usize::MAX`, but finite
values are a supported contract. Nous uses one in its paged external worker;
Basis exposes the value through its public compaction configuration; and
persisted Mentra agents retain an explicit finite value across resume. Changing
the meaning of the field would therefore change existing requests without a
host opting into a new policy.

The projection was previously invisible through the normal host surfaces. A
wrapping `Provider` could inspect the final `Request`, but `AgentEvent`,
`RuntimeHookEvent`, and `SessionEvent` carried no indication that content had
been replaced. This matters because the result remains present in the canonical
transcript while the model no longer receives its body.

Micro-compaction is not literally the only transient request transform. Memory
recall appends request-only context, and teammate identity can prepend it. It is
the only main-request transform identified here that silently destroys existing
provider-neutral message content after canonical history has been built.

## Decision

1. **Keep the existing finite policy unchanged.** A finite
   `keep_recent_tool_results` continues to preserve a contiguous recent suffix
   and replace eligible older payloads wholesale. `usize::MAX` remains the
   default and disables the rewrite.

2. **Keep oldest-first selection.** This follows the field's stated contract,
   is monotonic as history grows, and never makes a previously old result full
   because a newer large result arrived. It is not a claim that age measures
   importance. Evidence-bearing agents should leave the policy disabled.

3. **Emit typed, best-effort observability.** Every freshly built logical
   request that actually replaces at least one payload emits
   `AgentEvent::RequestToolResultsElided`. The event is mapped to the distinct
   `SessionEvent::RequestToolResultsElided` so session-based hosts such as Basis
   can forward it through their event sinks. Before the 0.22 release, ADR-0006
   generalizes the payload to identify the active policy, exact aggregate
   canonical/projected bytes, and ordered call id, optional tool name, error
   status, content kind, action, and byte counts for every changed result. It
   never copies the content into the event stream.

4. **Define the event per logical request projection.** Auto-compaction may
   build the same projection for estimation, but that estimate emits nothing.
   Transport retries reuse an already-built request and emit no duplicate.
   Rebuilding after canonical compaction is a new projection and may emit a new
   event.

5. **Do not make observation an override.** `AgentEvent` delivery is
   best-effort and cannot veto or rewrite the current request. ADR-0006 adds a
   static host-selected budget to configuration; a per-result retention pin or
   runtime callback would still require a separate synchronous policy seam with
   an explicit rule for protected content that exceeds the budget.

6. **Treat the event variants as a 0.22.0 API change.** Mentra 0.21.0 is already
   published, and both public event enums intentionally make exhaustive
   downstream matches fail when a new event needs handling. The change must not
   ship as a 0.21.x patch.

## Consequences

- A raw agent subscriber can log exactly which calls were reduced in each
  projected request. A session subscriber receives the same facts in the
  session event vocabulary.
- `ContextCompacted`, `CompactionStarted`, and `CompactionCompleted` retain
  their existing meaning: the canonical transcript was replaced by a summary.
  Request-only elision never reuses those events.
- Basis must add a conscious `SessionEvent` mapping and corresponding public
  event before adopting Mentra 0.22. Its existing `EventSink` trait needs no
  signature change.
- The tagged session-event vocabulary also grows. An older serialized-event
  reader that does not know the new tag will reject that event.
- The result list repeats every changed result for each changed projection. It
  can grow with a long finite-window history. That cost is accepted because an
  aggregate or capped event cannot answer which calls the current request lost;
  hosts that journal events may impose their own storage policy.
- A finite count is still not a byte or token bound. The newest `N` results can
  be arbitrarily large, old payloads at or below 100 bytes survive, markers
  accumulate, and non-tool content is unaffected.

## Rejected

- **Remove the finite knob.** Rejected because finite-window/disposable-result
  workloads and persisted configurations use it deliberately.
- **Redefine finite values as head/tail truncation.** Rejected because it would
  silently enlarge existing requests, change auto-compaction timing, grow with
  every old result, and can turn structured JSON into malformed fragments.
  Mentra's output limiter and paging layer already own partial-access policies.
- **Select by size instead of age.** Rejected because size is no better a proxy
  for importance, violates the named recent-suffix invariant, and can make the
  selected set unstable as new results arrive.
- **Read a magic retention key from `ToolOutput::details`.** Rejected because
  details are opaque host metadata and are absent from the `Message` projection
  the algorithm receives.
- **Reuse canonical compaction events.** Rejected because it would falsely tell
  hosts that stored history changed.
- **Add only a `RuntimeHookEvent`.** Rejected because Basis does not consume that
  seam, and a fallible audit hook would give observation new request-failure
  semantics.
