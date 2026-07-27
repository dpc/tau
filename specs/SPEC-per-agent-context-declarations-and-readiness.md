# SPEC-per-agent-context-declarations-and-readiness: Correlated per-agent context

## Record justification

The contract spans protocol fields, client helpers, generic admission and
interception, extension activation, prompt projection, initialization waits,
disconnect handling, and shell production; no component-local artifact owns all
of it.

This specification implements the per-agent context row of
[SPEC-peer-event-publication](SPEC-peer-event-publication.md)
and is constrained by
[SPEC-session-discovery-declarations-and-readiness](SPEC-session-discovery-declarations-and-readiness.md).

## Correlation and authority

Every `session.agent_loaded` carries a fresh mandatory
`agent_initialization_id`. Every authenticated configured local extension kind
may publish `extension.context_provider_register`,
`extension.agent_discovery_snapshot_declared`,
`extension.agent_context_publish`, and `extension.context_ready`; registration
is not an admission prerequisite. Mutating current state additionally requires
the exact session, agent, and initialization id.

Generic Emit captures the stable configured publisher and exact live connection
generation before ordinary same-name interception. Drop has no downstream effect;
replacement repeats structural and authority checks. A stale generation may
remain observable after commit but cannot mutate current state.

## Projection and readiness

A committed correlated context value replaces the connection's contribution for
its `(agent, key)` slot during initialization and remains valid for the same
frozen live initialization afterward. Arbitrary or unloaded agents, wrong
sessions, wrong initialization ids, and old load attempts cannot receive context.
Disconnect removes that connection's keyed contributions.

The per-agent wait set contains registered live non-socket Tool connections whose
live selectors match the exact `session.agent_loaded`. Only matching
`extension.context_ready` removes its source. Per-agent readiness never releases a
session-discovery wait. Duplicate, wrong-scope, stale, and unregistered readiness
is inert. Disconnect removes its source from every pending wait and may finalize
an initialization.

The single interception queue preserves declaration-before-readiness order.
Pre-Ready registrations, context values, and discovery snapshots use bounded
activation reservations; readiness remains operational traffic behind activation.

All raw events default to `persist=false`, remain excluded from semantic journals,
and have no cold or historical replay. The durable initialization replacement and
transient current projection are specified by
[SPEC-session-discovery-declarations-and-readiness](SPEC-session-discovery-declarations-and-readiness.md).

The local configured-extension trust boundary and bounded-wait risk are documented
in [`SECURITY.md`](../SECURITY.md).
