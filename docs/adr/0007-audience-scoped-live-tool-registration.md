# ADR-0007 — Add audience-scoped live tool registration

> Status: Accepted (2026-09-01)
> Created: 2026-09-01
> Tracks: [Issue #47](https://github.com/oops-rs/mentra/issues/47)
> Relates to: [ADR-0004 — Add agent-scoped host orchestration
> primitives](0004-host-orchestration.md)

## Context

A host may keep one Mentra runtime alive while serving several workspaces,
tenants, plugins, or other live execution contexts. A runtime-global custom
tool registry cannot safely represent that arrangement: registering a tool for
one context exposes it to every agent, while rebuilding a runtime for each
context loses shared providers, stores, hooks, skills, and running sessions.

A name alone is also insufficient ownership. Two contexts may intentionally
provide different implementations of `search`, and a context may close while a
same-name replacement is installed elsewhere. Name-only removal can then
delete the replacement (an ABA race), and separately reading a descriptor and
handler can schedule one generation under another generation's metadata.

The runtime already has exact-agent intrinsic tools, including reserved
terminal output and the live `read_tool_result` pager. Those tools need stronger
precedence than host audience tools, but they must not require a parallel map
whose lock can drift from the public registry. Tool profiles remain useful for
choosing a smaller model-facing surface, but a name allowlist cannot establish
which workspace or tenant owns a tool.

## Decision

### 1. Model an audience as opaque live identity

`ToolAudience` is an equality- and hash-comparable opaque string. Mentra does
not interpret it as a path, project id, session id, permission scope,
credential, or capability. Possessing or guessing its string is not an
authorization decision; hosts remain responsible for assigning the right
audience when they create or resume work.

The audience is attached to a derived `RuntimeHandle`, not `AgentConfig`. It is
therefore live and ephemeral: it is never serialized with an agent. Existing
creation without an audience and existing resume without an override remain
global-only. An audience must be supplied explicitly through raw-agent or
session creation/resume APIs each time persisted work is reopened.

Ordinary runtime-handle cloning carries the audience. Disposable subagents and
persistent teammates consequently inherit their parent's audience, including
when a disposable template replaces its `ToolProfile`.

### 2. Keep every namespace in one generation-addressed registry

The registry holds three namespaces and resolves one coherent snapshot in this
order:

1. the exact agent's intrinsic registrations;
2. registrations for the agent's matching `ToolAudience`;
3. runtime-global registrations.

The first matching name wins. Exact-agent registrations let independent agents
own same-name intrinsic tools. Audience registrations let different audiences
own different implementations with the same name. Global tools remain visible
to every audience unless an exact-agent or matching-audience registration has
the same name.

Provider rosters use the same precedence and are name-sorted after resolution.
Admission resolves the handler and immutable descriptor together under one
registry read, and later scheduling, authorization, and execution use that
generation-addressed snapshot. A replacement therefore cannot cause a call
classified from generation A to execute generation B.

### 3. Make collision behavior explicit

`Runtime::try_register_tool_for_audience` rejects a same-name global tool and a
same-name tool already in that audience. It permits the same name in another
audience. `Runtime::try_register_tool`, the safe global path, rejects a
same-name occupant in any global, audience, or exact-agent namespace.

The legacy infallible `Runtime::register_tool` and builtin global registration
retain replacement behavior. Installing their global entry atomically evicts
every same-name audience and exact-agent entry so the replacement is genuinely
global. The evicted entries' guards become stale and cannot remove the new
global entry.

The existing `Runtime::tools`, `Runtime::tool_descriptor`, and
`Runtime::unregister_tool` APIs remain global-only. They neither reveal nor
remove audience or exact-agent registrations. Audience registrations are
inspected through their guard metadata and removed through their guard.

### 4. Bind registration lifetime to a non-cloneable RAII guard

Successful audience registration returns a `#[must_use]`, non-`Clone`
`AudienceToolRegistration`. The guard records the audience, exact monotonically
unique generation, and the descriptor snapshot used for insertion. A tool's
descriptor is evaluated exactly once per registration, before the registry
write lock is acquired; `descriptor()` returns that stored snapshot.

Dropping the guard or consuming it with `unregister()` conditionally removes
only that exact audience/name/generation. `unregister()` reports whether it
removed the live entry. A stale guard is harmless after replacement, global
eviction, or runtime destruction, and the guard's weak reference does not keep
the runtime alive. Lock poisoning during guard cleanup does not turn `Drop`
into a panic. Removed or displaced user handlers are detached under the lock
and destroyed only after it is released, allowing reentrant destructors.

An admitted call owns a clone of its resolved handler and descriptor snapshot.
Dropping the registration prevents future admission but does not cancel a call
already in flight; that call may finish against the generation it resolved.

Internal exact-agent registrations use the same generation receipt and
non-cloneable RAII discipline. Reserved terminal tools retain both their guard
and exact gate identity. Paging agents retain their own `read_tool_result`
guard for their live lifetime; non-paging agents and other paging agents cannot
see or remove it.

### 5. Share the live registry, then narrow the visible surface

Derived runtime handles, sessions, disposable subagents, and teammates share
one live tooling authority. A matching session created before an audience tool
is registered observes its later registration and guard-driven removal without
being rebuilt.

`ToolProfile` is applied after namespace resolution. It can narrow the tools
already available to an agent, but it cannot grant a foreign audience's tool
or prove ownership, permission, or security provenance. Runtime policy,
authorization, and host audience assignment remain separate controls.

If a model guesses a name that exists only for another agent or audience,
resolution marks the call unavailable before pre-execution hooks,
authorization, or the tool handler runs. This prevents downstream policy code
from accidentally treating a foreign-name probe as an otherwise valid call.

## Consequences

- A process can safely multiplex live tool implementations over one runtime
  without leaking their rosters or calls across audiences.
- Same-name registrations across audiences are intentional and deterministic;
  the exact-agent, audience, and global precedence is shared by roster and
  execution paths.
- Hosts must retain each registration guard for as long as the tool should be
  callable and must reattach the audience on every persisted resume.
- Audience identity is routing context, not authentication. A host that derives
  it from untrusted input without validating ownership defeats its own
  isolation policy.
- `SessionOptions` gains a source-breaking field for exhaustive struct literals;
  `..Default::default()` preserves the previous global-only behavior.

## Rejected

- **Persist the audience in `AgentConfig`.** Rejected because a stale stored
  tenant or workspace identity could silently regain live tools after resume,
  and because routing identity is not durable agent behavior.
- **Use `ToolProfile` as the audience boundary.** Rejected because profiles are
  name filters, can be replaced for descendants, and cannot distinguish two
  implementations with the same name.
- **Keep one registry per session or clone registry snapshots.** Rejected
  because already-created sessions would not observe live registration and
  removal, and descendants could diverge from their parent authority.
- **Remove registrations by name alone.** Rejected because an old owner could
  remove a newer same-name generation.
- **Let safe global registration shadow scoped occupants.** Rejected because a
  loader that did not intend to replace a context-specific tool would silently
  redirect calls. Deliberate legacy/builtin replacement remains available with
  explicit eviction semantics.
- **Cancel admitted calls when their guard drops.** Rejected because admission
  already owns an immutable handler/descriptor snapshot; cancellation would
  require a separate execution-lifetime protocol and would not make future
  visibility safer.
