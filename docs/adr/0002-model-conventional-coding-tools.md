# ADR-0002 — Add model-conventional coding tools over the shared workspace engine

> Status: Proposed
> Created: 2026-07-16
> Precedent: [ADR-0001 — Evolve mentra into a pi-shaped pluggable agent core](0001-pluggable-agent-core.md).

## Context

mentra is an agent runtime library and owns the sole inner think/tool/observe
loop. Its builtin filesystem surface has historically been one batched `files`
tool. That surface provides ordered staging and cross-operation rollback, but
many model families have been trained to call conventional single-purpose
tools named `read`, `ls`, `grep`, `glob`, `write`, and `edit`. Hosts currently
have to translate those calls or register duplicate filesystem implementations.

The filesystem invariants already belong to `WorkspaceEditor`: path
normalization, runtime-policy authorization, overlay-aware reads, and atomic
commit. A second implementation would create two policy boundaries and allow
their behavior to drift. Investigation also found that recursive list/search
walks authorized only their root; a descendant symlink could resolve outside
the configured read roots before being traversed.

The change must preserve ADR-0001's additive contract. Existing runtimes must
continue to advertise and execute the batched `files` tool by default. Opaque
edit diagnostics must survive locally without crossing the provider projection
boundary, and mentra must remain a library rather than acquiring a CLI or host
workflow.

## Decision

1. mentra adds six builtin tool executors named `read`, `ls`, `grep`, `glob`,
   `write`, and `edit`. They are thin adapters over `WorkspaceEditor`; they do
   not duplicate path, policy, overlay, or commit logic. `move` and `delete`
   remain available through the batched tool.

2. A new public `FileToolProfile` selects `Batched`, `Split`, or `Both`.
   `Batched` is the default and preserves the existing provider tool list.
   `RuntimeBuilder::with_file_tools` reconfigures the eagerly registered tool
   registry immediately, so repeated selections have deterministic replacement
   semantics and `Runtime::empty_builder()` can explicitly opt into a file-tool
   surface.

3. `read`, `ls`, `grep`, and `glob` have a static
   `ReadOnlyParallel` execution category. `write` and `edit` have a static
   `ExclusiveLocalMutation` category. All six retain runtime-policy
   authorization inside `WorkspaceEditor`; tool metadata is not treated as a
   security boundary.

4. Recursive list, search, and glob operations use one walker. It reauthorizes
   each descendant before inspection or traversal and rejects a symlink whose
   target leaves the allowed read roots. Canonical visited-directory tracking
   still terminates in-root symlink loops.

5. Model-facing schemas use snake_case. Deserializers accept the documented
   camelCase aliases, `path`/`file_path`, the legacy top-level single-edit
   shape, and `edits` encoded as a JSON string. Input normalization ends at this
   adapter layer; `WorkspaceEditor` receives typed operations.

6. `grep` supports optional file globs, literal matching, case folding,
   context, and multiline regular expressions. Every rendered physical line is
   capped at 500 Unicode scalar values without splitting UTF-8. `glob` reuses
   the authorized recursive walker and the existing `glob-match` dependency.

7. The opt-in split `edit` tool matches every edit against the original file,
   rejects missing, ambiguous, overlapping, and no-op edits, and applies
   replacements in reverse byte-range order. It normalizes UTF-8 BOM and CRLF
   for matching and restores both on commit. Exact matching is tried first; a
   fallback normalizes NFKC, trailing whitespace, smart quotes, and Unicode
   dashes while preserving unchanged original lines. The provider receives only
   a short replacement summary. Display diff, unified patch, and
   `first_changed_line` are stored in opaque `ToolOutput::details` and never
   projected to a provider.

8. The historical batched `files.replace` implementation is not routed through
   the new fuzzy edit engine. Its exact-match and expected-replacement behavior
   remains unchanged. The descendant-symlink rejection is the one intentional
   default-path change because it enforces the already-declared policy roots.

## Consequences

- Models can call the tool vocabulary they were trained on without a host
  translation layer, while hosts that need staged multi-file transactions keep
  the existing batched tool.
- Both surfaces share one authorization and commit engine, so fixes apply at a
  single filesystem boundary. Recursive traversal can no longer escape through
  a descendant symlink.
- Read-only calls can execute concurrently and still return results in provider
  call order; mutations remain serialized by the existing orchestrator.
- Edit diagnostics remain durable local metadata for host rendering and replay,
  but provider requests stay compact and cannot observe those details.
- Embedders see no new tools unless they select `Split` or `Both`. Selecting a
  profile reserves the seven builtin file-tool names (`files` plus the six split
  names) in that builder's registry.

## Rejected

- **Replace the batched `files` tool with split tools.** Rejected because it
  would break existing provider prompts and remove ordered staging and
  cross-file rollback.
- **Make `Split` the default.** Rejected because changing the advertised tool
  list changes model behavior even when source compatibility is preserved.
- **Implement each split tool directly against `std::fs`.** Rejected because it
  would duplicate authorization, symlink, overlay, and atomic-write invariants.
- **Send edit diffs and patches in provider-visible result content.** Rejected
  because large diagnostics consume model context and ADR-0001 already provides
  the local opaque-details carrier.
- **Add `move` and `delete` split tools now.** Rejected as unnecessary for the
  model-conventional minimum; the batched tool and authorized shell surface
  already cover those operations.
- **Use shell commands to implement grep and glob.** Rejected because behavior
  would become platform-dependent and would bypass the shared file-policy
  engine.
