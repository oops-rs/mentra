# ADR-0006 — Bound projected tool-result bodies with an opt-in byte budget

> Status: Accepted (2026-08-26)
> Created: 2026-08-26
> Extends: [ADR-0005 — Make request-only tool-result elision observable](0005-observable-request-tool-result-elision.md)

## Context

Mentra already has three distinct controls over tool output:

1. The runtime output limiter bounds each result when it enters canonical
   history. Text keeps head and tail; oversized structured JSON becomes a whole
   text notice rather than an invalid fragment.
2. Optional paging replaces an oversized text result with its first window and
   retains the post-limiter, post-hook text in live agent memory for later
   `read_tool_result` calls.
3. A finite `CompactionConfig::keep_recent_tool_results` marker-elides eligible
   old result bodies in every main request projection. Its default is
   `usize::MAX`, which disables that legacy projection.

The default configuration therefore already has the desirable ordinary-history
invariant: per-result output is bounded at ingestion, and results are not aged
out later. The remaining gap is an opt-in aggregate guard for hosts that know
the total tool-result content in one request must remain below a fixed amount.
The legacy recent-count policy is not such a guard: recent results are
unbounded, old short results survive, and markers accumulate.

A strict aggregate cap conflicts with two attractive guarantees. One recent
result may exceed the entire cap, so a protected newest suffix cannot also be
unconditionally whole. Likewise, a nonempty marker for every historical call
has an `O(number of calls)` floor that can itself exceed any fixed budget. The
configuration must state which invariant wins.

Current paging cannot close the gap by itself. It retains only text, only in
live per-agent memory, after limiting and hooks. Resume reconstructs no retained
payload; structured values never enter that store; and one overlong line is
hard-cut rather than byte-pageable. A new projection must not synthesize a
reader reference it cannot honor.

## Decision

### 1. Add one optional, exclusive budget mode

`CompactionConfig` gains:

```rust
pub projected_tool_result_budget: Option<ProjectedToolResultBudget>
```

with:

```rust
pub struct ProjectedToolResultBudget {
    pub max_bytes: usize,
    pub prioritize_recent_results: usize,
    pub max_preview_bytes: usize,
}
```

The option defaults to `None` and is omitted from serialized defaults. `None`
runs `keep_recent_tool_results` exactly as before. `Some` selects budget mode
exclusively; the persisted legacy count remains present but is not consulted.
The two algorithms are never applied sequentially.

The budget type has no `Default`. Mentra has no first-principles basis for
choosing lossy byte counts for a host; all three values must be explicit.

### 2. Define the hard cap narrowly and exactly

`max_bytes` is a hard cap on the sum of final
`ToolResultContent::len()` values for every `ContentBlock::ToolResult` in the
provider-neutral message projection, under every message role.

It includes whole bodies, generated previews, descriptive markers, Unicode
ellipsis fallbacks, and empty bodies. It excludes roles, call ids, message and
JSON framing, tool arguments and definitions, images, system/user/assistant
text, provider escaping, and provider-specific prefixes. It is therefore a
projected tool-result-content budget, not a total request or wire-size limit.

`max_bytes = 0` is valid: every nonempty tool-result body becomes empty while
the `ToolResult` blocks and call pairing remain.

### 3. Prefer broad omission honesty before richer recent content

When the canonical total already fits, the projection is byte-identical and no
event is emitted. Otherwise, collect every tool-result block in request order
and allocate in two passes.

The floor pass walks newest to oldest. For each result:

1. Keep the original body if it is no larger than its normal
   `[Previous: used <tool>]` marker.
2. Otherwise use that descriptive marker when it fits.
3. Otherwise use the three-byte Unicode ellipsis `…` when it fits.
4. Otherwise use empty text.

This pass spends from the aggregate cap. It means one recent whole body does
not consume the budget before older call/result pairs can receive an honest
marker; the recent result's own floor is still allocated first. When even the
marker floor cannot fit, lower-priority older results degrade to ellipsis or
empty bodies. No generated content lives outside the named cap.

The upgrade pass has three newest-to-oldest tiers:

1. Restore the newest `prioritize_recent_results` to their whole canonical body
   when each incremental cost fits.
2. Upgrade changed text results to UTF-8-safe
   head/omission-separator/tail previews. `max_preview_bytes` caps the entire
   generated preview, including its separator. Zero disables this tier.
3. Spend any remaining budget restoring historical results to their whole
   canonical body when each incremental cost fits.

This ordering makes recent whole bodies and broad text previews more valuable
than restoring one old large body, without making any body permanently
ineligible for full retention. `max_preview_bytes` constrains generated previews,
not an unchanged whole body.

Structured content is atomic. It is either the original valid JSON value or
whole text marker, ellipsis, or empty text. No JSON fragment is emitted.

For a text result, define `preview_limit` as the minimum of
`max_preview_bytes`, `current_body_bytes + remaining_budget`, and one byte less
than the canonical body (when the canonical body is nonempty). The preview
separator is exactly `\n…[omitted]…\n`. The bytes in `preview_limit` left after
that 17-byte separator are split evenly (the head receives the odd byte), then
rounded down to complete UTF-8 scalar boundaries for the longest fitting prefix
and suffix. The unused rounding slack is deliberately not rebalanced.

A preview is valid only when it retains at least one complete scalar at each
end, omits at least one complete scalar between them, is shorter than the
canonical body, and is strictly longer than the result's current floor. If any
condition fails, the floor remains. Thus every upgrade is monotonic in rendered
length: previews never replace a more informative, longer descriptive marker
and never refund bytes after recent full-body priority has been applied.

Actual rendered byte deltas, not requested allocations, are charged. Bytes
left unused by a UTF-8 boundary or an atomic structured marker remain available
to the next result.

`prioritize_recent_results` grants upgrade priority; it is neither an exemption
from `max_bytes` nor a promise that those results remain whole.

### 4. Keep the projection pure and recovery-neutral

Budgeting operates only on cloned request messages. It does not mutate the
canonical transcript, register tools, retain payloads, create spill files,
parse paging trailers, or synthesize `read_tool_result` references.

An existing paging trailer is ordinary canonical text. A head/tail preview may
preserve it byte-for-byte, but budget mode does not promise that the live pager
still has the target. Paging remains separately opt-in, text-only,
post-limiter/post-hook, and live-agent-only.

### 5. Generalize the unreleased projection event

ADR-0005's event has not shipped. Before 0.22, generalize it rather than adding
one event variant per policy.

`RequestToolResultsElided` identifies the active policy (legacy recent count or
projected byte budget), exact canonical and projected aggregate body bytes, and
each changed result in request order. A changed-result record carries call id,
optional tool name, error flag, source kind (text or structured), action
(preview, marker, or omitted), and canonical/projected byte counts. It carries
no body snippet, path, retrieval flag, or reader arguments.

`Preview` retains canonical text around an omission separator. `Marker` retains
no canonical body bytes but names the earlier tool. `Omitted` is only an opaque
ellipsis or empty body.

As in ADR-0005, estimation emits nothing, one freshly built logical request
emits at most one event, transport retries do not duplicate it, and a rebuild
after canonical compaction is a new projection.

### 6. Preserve upgrade compatibility and document downgrade failure

The optional field uses `serde(default)` so pre-0.22 persisted agents load with
budget mode disabled. Omitting `None` preserves default and legacy JSON shapes.
Every explicit finite legacy count keeps its exact behavior.

Adding the public field and generalizing public exhaustive event payloads is a
0.22 source change. A downgrade is not safe for a budgeted persisted agent:
Mentra 0.21 and earlier ignore the unknown field and apply the stored legacy
count, commonly `usize::MAX`, silently removing the cap. Budgeted persisted
agents therefore require Mentra 0.22 or later.

## Consequences

- Opted-in hosts get a deterministic, testable hard aggregate bound over the
  exact provider-neutral tool-result bodies Mentra projects.
- Default agents, evidence agents, and every existing finite legacy
  configuration behave unchanged.
- The cap cannot be presented as a total request limit or as a durable recovery
  mechanism.
- The marker-floor rule favors per-call omission honesty before richer previews;
  recent priority applies to upgrades.
- Global append-monotonicity is not promised. Appending a result changes the
  recent-priority set and can redistribute a stateless byte budget.
- Basis can expose the `Copy` budget configuration additively, but adopting
  Mentra 0.22 still requires an exhaustive session-event mapping and a decision
  about its own serialized event schema.

## Rejected

- **Change the default.** Rejected because default ingestion limiting plus
  keep-all history already provides the safer evidence behavior.
- **Redefine `keep_recent_tool_results`.** Rejected because persisted and
  downstream callers rely on its exact marker and recent-suffix semantics.
- **Let recent results overflow the cap.** Rejected because it turns a hard
  operational guard back into a heuristic.
- **Require a marker for every result.** Rejected because the marker floor can
  exceed the configured cap. Ellipsis and empty fallbacks are necessary.
- **Spend everything on the newest result first.** Rejected because older
  tool-result pairs would become silently empty even when small descriptive
  markers for several calls could fit.
- **Slice structured JSON.** Rejected because invalid fragments misrepresent
  the provider-neutral structured-content contract.
- **Couple budgeting to paging or spills.** Rejected because those mechanisms
  have different lifetime, type, and durability guarantees.
- **Claim append-monotonic allocation.** Rejected because a finite recent set
  necessarily changes membership when history grows; a freed allocation can
  change an older preview.
