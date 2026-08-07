# Automatic Tool-Result Paging

This document proposes a built-in mentra feature: when a tool returns an
oversized result, the agent transcript receives only the first page plus an
honest continuation marker, while a built-in `read_tool_result` tool lets the
model read further windows on demand. Small results are delivered exactly as
today. The feature is opt-in per agent and changes nothing for existing
consumers until enabled.

Status: proposed (2026-08-07). Not yet implemented.

---

## Motivation

An agent loop cannot control the size of a tool result — especially when the
tool wraps an external MCP server the operator does not own. A single
oversized result, or several ordinary-sized ones accumulating, overflows the
model's context window and kills the run with a provider error the agent can
neither prevent nor recover from.

This is not hypothetical. A production consumer (Nous's isolated
external-evidence worker) ran a log-retrieval MCP server through mentra:
eighteen fetches accumulated 1.82 MB of tool results in one context —
individual results reached 445 KB — and the next provider call failed with
`context_length_exceeded` before the worker could produce its typed finish.
Every consumer-side mitigation available today is lossy or partial:

- **Per-result byte caps** (server- or adapter-side) truncate the tail
  permanently; a result the model legitimately needs to read deeply is
  simply gone.
- **Compaction** (`CompactionConfig`) manages *old* context, and its
  micro-compaction (`keep_recent_tool_results`) elides results the model may
  not have finished using.
- **Per-tool cooperation** (each server implementing its own windowing)
  cannot be assumed; the whole point of MCP is that servers are third-party.

The transcript-insertion boundary inside mentra is the one choke point that
covers every tool of every agent with no per-tool cooperation. Managing what
*enters* the context is the natural sibling of compaction, which already
lives here and manages what *stays*.

## Design overview

Three rules:

1. **Below the threshold nothing changes.** A result smaller than
   `threshold_bytes` is inserted byte-identical to today. The pager is
   invisible until it is needed.

2. **Above the threshold the transcript gets page 1, not the result.** The
   inserted `ToolResult` content is the first `page_bytes` of the result,
   cut on a line boundary, followed by a structured trailer:

   ```text
   …[paged: lines 1–812 of 5,723 (64.0 KB of 410.3 KB). Call
   read_tool_result(tool_use_id="call_8", start_line=813) for the next
   window.]
   ```

   Line numbers are **absolute over the full result** on every page, so
   anything the model quotes or cites by line survives paging unchanged.

3. **The event stream keeps full fidelity.** `AgentEvent::ToolExecutionFinished`
   carries the complete, unpaged `ContentBlock::ToolResult` exactly as today.
   Consumers that reconstruct evidence, ledgers, or telemetry from events
   (the Nous evidence pipeline does exactly this) observe no change
   whatsoever. Paging shapes the *model's view*, never the record.

### The built-in `read_tool_result` tool

Registered automatically on the agent iff paging is enabled — the same
agent-scoped registration path the team/subagent built-ins use. Schema:

```json
{
  "name": "read_tool_result",
  "input": {
    "tool_use_id": "the id printed in the paging trailer",
    "start_line":  "1-based absolute line to start the window at"
  }
}
```

Behaviour:

- Returns the window starting at `start_line`, at most `page_bytes`, cut on
  a line boundary, with the same trailer format (or `…[end of result]`).
- Serves **only results recorded by this agent's own run** — an unknown
  `tool_use_id` is an ordinary tool error, and one agent can never read
  another agent's results.
- Its own results are bounded by construction (`page_bytes`) and are never
  paged recursively.
- It costs a tool call from the run's tool budget — that is the honest
  price of context growth — but it never re-executes the original tool, so
  side-effectful or expensive tools are not re-triggered by reading.

### Configuration

```rust
pub struct ToolResultPagingConfig {
    /// Results at or below this size are inserted whole. Default 64 KiB.
    pub threshold_bytes: usize,
    /// Maximum bytes per inserted page/window. Default 32 KiB.
    pub page_bytes: usize,
}

pub struct AgentConfig {
    // …
    /// `None` (default) preserves today's behaviour exactly.
    pub tool_result_paging: Option<ToolResultPagingConfig>,
}
```

`threshold_bytes >= page_bytes` is not required but is the sensible posture:
a result slightly over the threshold still arrives mostly whole as one large
first page would defeat the purpose.

### Storage

Full results for the current run are retained in an in-memory per-agent map
`tool_use_id -> Arc<str>` populated at insertion time, dropped when the
agent is dropped. This is strictly less memory than today's behaviour (today
the full text lives in the transcript *and* is replayed to the provider on
every round; with paging it lives in the map once and only pages enter the
transcript). Persistence across process restarts is a non-goal: the pager
serves the live run, and mentra's transcript persistence already captures
what the model actually saw.

## Where it hooks in

- `tool/orchestrator.rs` executes calls and emits
  `AgentEvent::ToolExecutionFinished { result }` (multiple sites; all emit
  the full block today — unchanged). The paging transform applies to the
  `ContentBlock::ToolResult` *after* event emission, before the block joins
  the round's committed tool-result message consumed by the runner
  (`agent/runner.rs`, `summarize_tool_results`).
- `agent/config.rs` gains `ToolResultPagingConfig` beside
  `CompactionConfig`.
- The `read_tool_result` registration follows the existing built-in tool
  pattern.

## Interactions

- **Compaction**: a page is an ordinary tool result; micro-compaction elides
  old pages exactly as it elides old results. The two compose: paging bounds
  each insertion, `keep_recent_tool_results` bounds how many insertions
  stay.
- **Parallel tool calls**: pages are per-`tool_use_id`; six parallel
  oversized results each arrive as their own page 1. Worst-case insertion
  per round becomes `parallel_calls × page_bytes` instead of unbounded.
- **`is_error` results**: paged the same way; error text can be oversized
  too.
- **Non-text content**: only `ContentBlock::ToolResult { content: String }`
  is paged; nothing else is touched.

## What this deliberately does not do

- No token estimation. Bytes are cheap, deterministic, and close enough;
  the threshold is a guardrail, not an optimizer.
- No summarization. A page is verbatim source text; deciding what matters
  is the model's job, and summaries would poison citation line numbers.
- No cross-agent or cross-run reads.
- No change to any event, hook, or session-mapping payload.

## Testing

1. Sub-threshold result → transcript byte-identical, `read_tool_result`
   not registered when paging is `None`.
2. Oversized result → transcript holds page 1 + trailer; the
   `ToolExecutionFinished` event carries the full result.
3. `read_tool_result` windows: correct absolute line ranges, final window
   marked as end, `start_line` past EOF → empty window with end marker.
4. Unknown `tool_use_id` → tool error, run continues.
5. Line-boundary cuts: no split UTF-8, no split lines (a single line longer
   than `page_bytes` is the one case that must hard-cut — mark it).
6. Parallel oversized results page independently.

## Adoption plan

Nous adopts it first for isolated external-evidence workers
(`worker_agent_config`), where the incident occurred: paging on, threshold
64 KiB, pages 32 KiB. Consumer-side caps (per-server `max_result_bytes`)
remain as anti-abuse backstops, and server-side line clipping (xlog-mcp)
remains valuable against pathological single lines. Gather-class agents can
adopt later once the worker profile has soaked.
