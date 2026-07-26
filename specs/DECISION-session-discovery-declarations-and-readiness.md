# DECISION-session-discovery-declarations-and-readiness: Session-discovery provider readiness

Authority: confirmed, 2026-07-21, dpc

Session discovery providers publish
`extension.session_context_provider_register` and
`extension.session_context_ready` through ordinary generic `Emit` interception.
Each event must commit before the harness updates provider membership or releases
session initialization.

Atomic skill and AGENTS.md declarations, their session and agent scopes, and
their canonical projections are governed instead by
[DECISION-agent-initialization-discovery-snapshots](DECISION-agent-initialization-discovery-snapshots.md).
The serialized publication queue ensures an earlier session snapshot settles
before a later readiness event can commit.

Every authenticated configured extension entry, across all `ClientKind` variants
including `ClientKind::Core`, may author both events without a capability. The
authenticated configured entry, not a claimed `Hello.client_kind`, grants this authority.
Unconfigured/socket peers have none; harness-internal direct publication remains outside
peer admission.

Registration is not an admission prerequisite for readiness publication. Every
admitted non-dropped raw event remains observable. Effective wait participation is
deliberately narrower: only a
registered, live, non-socket `ClientKind::Tool` connection whose live selectors match
`session.started` by `EventSelector::Exact` or matching `EventSelector::Prefix` enters
the wait set. All other registrations, including Tool connections without a matching
live selector, are inert. A connected effective waiter intentionally has no readiness
deadline; acknowledgement or disconnect releases it.

The harness captures both the stable configured publisher (`ExtensionName`) and exact
run-local source (`ConnectionId`) before interception and revalidates the complete live
generation before downstream effects. Stable `ExtensionInstanceId` alone cannot
authorize work across respawn. A stale generation's committed event remains only an
observation and cannot mutate successor-generation state.

The two raw inputs, session-provider membership, and readiness wait correlation
are daemon-runtime-only. The raw events default to `persist=false`, are excluded
from semantic agent, session, and restore journals
regardless of either caller-supplied `Emit.persist` value, and have no cold restore,
historical replay, or current-state synthesis. First-party sends set wire
`persist=false`; raw callers retain their supplied bit as generic publication metadata.

## Rationale

`tau-ext-shell` emits its complete session snapshot before readiness on one
serialized writer. Commit-before-effects ordering prevents readiness from
overtaking an intercepted snapshot and completing required-skill preflight early.

## Alternatives and tradeoffs

- **Tool-only authorship or a new capability:** rejected; it would narrow current
  configured-extension publication without a demonstrated need.
- **Make every configured registrant a waiter:** rejected; it would expand startup
  blocking beyond current live Tool subscribers.
- **Gate declarations/readiness on registration:** rejected; raw observations remain
  publishable, while wait-set membership alone controls barrier release.
- **Add a readiness deadline:** rejected for this slice; it would change existing
  availability semantics and needs a separate decision.

The distributed contract is
[SPEC-session-discovery-declarations-and-readiness](SPEC-session-discovery-declarations-and-readiness.md).
This decision implements the corresponding row of
[DECISION-generic-peer-event-emission](DECISION-generic-peer-event-emission.md) under
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md).

Per-agent discovery snapshots and correlated agent readiness are governed by
[DECISION-agent-initialization-discovery-snapshots](DECISION-agent-initialization-discovery-snapshots.md).
This decision does not change unrelated prompt-fragment, Action, agent-start,
internal-prompt, metadata, shell, UI, state, terminal, tool/provider,
custom-event, publisher-envelope, or persistence work.
