# ADR-0003 — Preserve provider reasoning through neutral content blocks

> Status: Proposed
> Created: 2026-07-16
> Companion decision: [ADR-0001 — Evolve mentra into a pi-shaped pluggable
> agent core](0001-pluggable-agent-core.md).

## Context

mentra owns the sole model/tool loop, but its provider-neutral transcript has
no reasoning content type. Anthropic thinking blocks, OpenAI Responses
reasoning items, and Gemini thought parts are therefore discarded before a
committed assistant message is persisted. Responses' server-side
`previous_response_id` can hide that loss only while provider state remains
available; replay-only mode and hybrid fallback still need a complete local
transcript.

Reasoning payloads are not interchangeable. Anthropic signatures, Responses
encrypted content, and Gemini thought signatures are opaque provider data. A
payload accepted by one provider and model can be rejected by another. A safe
transcript must retain that data without interpreting it and must have a
deterministic fallback when exact replay is not possible.

## Decision

1. Add externally tagged `ContentBlock::Thinking` with text, opaque signature,
   encrypted content, reasoning-item id, provenance, and a redacted marker.
   `ReasoningProvenance` records the registered provider id, requested model,
   and `ReasoningFormat`. Optional fields are serde-defaulted and omitted when
   absent, so previously persisted messages and transcripts deserialize
   unchanged.

2. Carry thinking through `ContentBlockStart::Thinking`,
   `ContentBlockDelta::ThinkingText`, and metadata-only deltas. The existing
   index-keyed response and pending-turn accumulators preserve provider block
   order. Text deltas emit `AgentEvent::ReasoningDelta`, mapped to
   `SessionEvent::AssistantReasoningDelta`; opaque metadata never enters host
   text-delta events and becomes durable only when the block closes and the
   assistant message commits.

3. Replay opaque reasoning only for assistant-role history when provenance
   exactly matches the target registered provider id, requested model, and
   reasoning format. A normal Anthropic signature must be present and
   nonempty; mentra cannot otherwise validate an opaque signature locally.
   Missing metadata, user-role thinking, cross-provider history, cross-model
   history, and empty signatures downgrade to ordinary text. An opaque-only
   block uses a deterministic nonempty marker. Replay never skips the block or
   turns an otherwise valid transcript into a local error. Redacted Anthropic
   blocks replay their opaque data as `redacted_thinking.data`.

4. Implement providers in fidelity order. This change ships Phase 1, capturing
   and replaying Anthropic signed and redacted thinking, and Phase 2, capturing
   Responses reasoning output items and replaying them as reasoning input
   items. A request with reasoning enabled adds
   `reasoning.encrypted_content` to `include` exactly once. A reasoning-bearing
   response composite-encodes `call_id|function_item_id` locally so replay can
   restore the function item ID; ordinary Responses calls retain the historical
   plain `call_id`, function-call outputs always use the raw call ID, and a
   provenance downgrade omits the function item ID. Because Azure-compatible
   endpoints may add encrypted content only in final `response.output`, the
   stream model backfills it through an internal metadata delta without
   replacing output-item ciphertext or exposing it as reasoning text. Gemini
   thought capture and signatures on `ToolUse`/text parts remain deferred
   because full Gemini fidelity requires signature carriage on more than
   thinking blocks.

5. Persist thinking as ordinary message content. Required continuation tails
   and transcript snapshots therefore preserve it verbatim. Summarizer text
   extraction continues to use text blocks only, and the local summarizer's
   full-transcript JSON projection removes thinking blocks so private reasoning
   is not copied into a summary prompt.

6. Keep the change additive. Existing provider requests without thinking are
   unchanged. The new public enum variants are a source-compatibility concern
   only for exhaustive matchers and constructors.

## Consequences

- Anthropic tool loops can resume from local persistence without dropping the
  signed thinking blocks required by the API.
- Hybrid Responses fallback no longer depends on inaccessible server state for
  captured reasoning/tool exchanges.
- Cross-provider and cross-model transcript reuse remains valid because opaque
  payloads become ordinary text instead of being forwarded unsafely.
- Hosts can render live reasoning text separately from assistant answer tokens
  without receiving signatures or encrypted payloads.
- Persisted transcripts may contain sensitive reasoning metadata. Existing
  transcript storage protections apply; compaction does not echo it into local
  summarizer prompts.
- Gemini reasoning remains incomplete until signatures can be retained on
  `ToolUse` and text blocks as well as thinking blocks.

## Rejected

- **Store reasoning as ordinary text only.** Rejected because it loses the
  opaque payload needed for faithful same-provider, same-model replay.
- **Replay by provider family without model provenance.** Rejected because
  opaque payload acceptance can change across models and custom registered
  provider ids.
- **Treat a nonempty opaque signature as cryptographically validated.**
  Rejected because mentra has no provider verification key or validation
  contract; nonempty is only a wire-shape prerequisite.
- **Fail or skip when reasoning cannot replay.** Rejected because persisted
  history must remain usable across provider/model switches and older records.
- **Expose signatures in reasoning delta events.** Rejected because host events
  need display text, not provider credentials or opaque continuation data.
- **Implement Gemini thinking without `ToolUse` signature carriage.** Rejected
  as a claim of full fidelity; the partial implementation is deferred and its
  limitation is explicit.
