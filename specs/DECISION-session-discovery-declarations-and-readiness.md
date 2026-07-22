# DECISION-session-discovery-declarations-and-readiness: Migrate session-discovery declarations and readiness together

Authority: confirmed, 2026-07-21, dpc

Migrate exactly these four peer events as one cohesive harness-extension interface slice:

- `extension.session_context_provider_register`;
- `extension.skill_available`;
- `extension.agents_md_available`; and
- `extension.session_context_ready`.

Each event must pass ordinary generic `Emit` interception and commit before the harness
updates registration or discovery state, publishes derived diagnostics/transcript facts,
or releases session initialization. The existing single pending interception and FIFO
deferred-publication queue ensure that an earlier skill or AGENTS.md declaration settles
before a later readiness event can commit.

Every authenticated configured extension entry, across all `ClientKind` variants
including `ClientKind::Core`, may author all four events without a capability. The
authenticated configured entry, not a claimed `Hello.client_kind`, grants this authority.
Unconfigured/socket peers have none; harness-internal direct publication remains outside
peer admission.

Registration is not an admission prerequisite for skill declarations, AGENTS.md
declarations, their projection, or readiness publication. Every admitted non-dropped raw
event remains observable. Effective wait participation is deliberately narrower: only a
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

The four raw inputs, session-provider membership, skill candidate/winner state,
AGENTS.md file slots, and readiness wait correlation are daemon-runtime-only. The raw
events default to `persist=false`, are excluded from semantic agent, session, and restore journals
regardless of either caller-supplied `Emit.persist` value, and have no cold restore,
historical replay, or current-state synthesis. First-party sends set wire
`persist=false`; raw callers retain their supplied bit as generic publication metadata.
Derived skill notices and durable AGENTS.md `agent.user_message_injected` facts retain
their separate classifications.

## Rationale

`tau-ext-shell` emits skill declarations, then AGENTS.md declarations, then readiness on
one serialized writer. Migrating a skill or AGENTS.md declaration while readiness still
has legacy pre-commit effects would let readiness overtake the intercepted declaration
and complete session initialization or required-skill validation too early. One cohesive
migration eliminates that unsafe intermediate state and applies one consistent
commit-before-effects boundary across the session-discovery protocol.

The cohesive slice is wider than first migrating only registration/readiness, so it
requires broader projection, derived-fact, activation, and regression work. This cost is
accepted to complete the whole session-discovery transition in one implementation slice.

## Alternatives and tradeoffs

- **Registration/readiness first, skills and AGENTS.md later:** technically safe after the
  foundation, but rejected in favor of one cohesive migration.
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

This decision does not migrate `extension.context_provider_register`,
`extension.agent_context_publish`, or `extension.context_ready`. It does not change the
already-governed prompt-fragment flow or unrelated Action, agent-start, internal-prompt,
metadata, shell, UI, state, terminal, tool/provider, custom-event, publisher-envelope, or
persistence work.
