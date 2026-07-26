# SPEC-per-agent-context-declarations-and-readiness: Per-agent context publication

## Status

`AgentInitializationId` and the new agent discovery/replacement/projection DTOs
exist as additive migration scaffolding, but existing `session.agent_loaded`,
`extension.agent_context_publish`, and `extension.context_ready` payloads remain
uncorrelated and retain the behavior below. The next atomic runtime phase must
add mandatory initialization correlation while switching their producers and
consumers; the current overlap is not a supported final interface.

## Record justification

The contract spans protocol defaults, client helpers, generic harness admission
and interception, extension activation, prompt projection and waits, semantic
store exclusion, and first-party shell producers. No component-local artifact
can describe its complete authority, ordering, lifetime, and persistence rules.

This specification implements the per-agent context row of
[DECISION-generic-peer-event-emission](DECISION-generic-peer-event-emission.md).
It governs `extension.context_provider_register`,
`extension.agent_context_publish`, and `extension.context_ready`.

## Authority and publication

Every authenticated configured extension entry kind, including configured Core,
may author all three events without a capability. Unconfigured and socket peers
have no authority. Registration does not gate value or readiness publication,
and values may name an agent that is not currently loaded. These asymmetries
preserve the existing extension interface.

Generic Emit captures the stable configured publisher, exact run-local
`ConnectionId`, instance id, and kind before ordinary same-name interception.
Drop has no downstream effect. Replacement repeats structural and authority
admission. After commit, the consumer revalidates that complete generation
before changing provider membership, prompt context, or readiness barriers. A
stale generation's event remains observable but cannot mutate successor state.

## Projection and readiness

A committed registration adds its exact live connection to the provider set. A
committed value replaces that connection's contribution for its `(agent, key)`
slot. Disconnect removes the connection's provider membership and all of its
contributions. It also removes that exact source from every captured per-agent
wait set; removing the final waiter immediately resumes any prompt deferred at
the publish-idle dispatch boundary.

When the harness loads an agent, its wait set contains registered, live,
non-socket Tool connections whose live selectors match `session.agent_loaded`
by Exact or Prefix. Readiness publication itself remains ungated: only a source
already present in the captured wait set can release its entry.

For compatibility, a committed `extension.context_ready` with the current
session id performs both existing operations: it releases the source from the
named agent's per-agent wait and from any session-initialization wait containing
that source. This cross-scope release is intentional preservation, not a new
recommendation. A mismatched session id changes neither wait.

The single pending interception and FIFO deferred-publication queue ensure that
an earlier registration or value settles before a later readiness
acknowledgement can commit and release work.

## Activation and persistence

Pre-Ready registrations and values reserve bounded activation message count and
encoded bytes before interception. Replacement reaccounts bytes. Commit settles
the family pending count while retaining the declaration's charge in its
activation stage; activation removes the stage and its charges. Drop, oversize
failure, or disconnect release both the charge and pending count. Recorded Ready
cannot activate the extension while such a declaration remains unsettled.
Readiness is operational traffic and stays behind activation.

All three raw events default to `persist=false` and are unconditionally excluded from
semantic agent, session, and restore journals for either caller-supplied
`Emit.persist` value. They have no cold replay or current-state synthesis.
First-party registration, value, and readiness sends use `persist=false`.

The local configured-extension trust boundary and bounded-wait risk are
documented in [`SECURITY.md`](../SECURITY.md). Unrelated authority-matrix rows
remain outside this specification.

`agent.initialization_context_set` is the durable replacement DTO for an exact
session, agent, and initialization correlation. It carries the optional
bootstrap message, frozen effective skills, and ordered AGENTS.md summaries.
No reducer folds it during this scaffold phase. The corresponding
`harness.agent_context_initialized` current projection remains transient and
unpublished until the runtime switch.
