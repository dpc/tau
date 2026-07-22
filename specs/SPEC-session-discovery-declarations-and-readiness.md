# SPEC-session-discovery-declarations-and-readiness: Session-discovery declarations and readiness

## Record justification

Session discovery spans protocol defaults, generic peer admission and interception,
extension activation, harness-owned skill and AGENTS.md projections, session readiness,
client helpers, and the shell producer. No one component can document the complete
authority, ordering, lifetime, and persistence contract.

This specification implements
[DECISION-session-discovery-declarations-and-readiness](DECISION-session-discovery-declarations-and-readiness.md)
and the session-discovery row of
[DECISION-generic-peer-event-emission](DECISION-generic-peer-event-emission.md).

## Authority and publication

Every authenticated configured extension entry kind, including configured Core, may
publish `extension.session_context_provider_register`, `extension.skill_available`,
`extension.agents_md_available`, and `extension.session_context_ready` without a
capability. Unconfigured/socket peers have no authority; harness-internal publication
remains outside peer admission. Registration does not gate publication or projection of
the other three events.

Generic Emit captures the stable configured publisher and exact run-local source
`ConnectionId`, configured instance id, and kind before ordinary same-name interception.
Every surviving event commits and broadcasts before downstream work. Replacement cannot
change event name or publisher and repeats structural/authority admission. Drop performs
no projection, diagnostic, injection, registration, or readiness work. A committed stale
generation remains observable but cannot mutate current state.

## Registration and readiness

Session-provider registration is idempotent membership keyed by source `ConnectionId`.
Repeated non-dropped registrations all commit; only membership coalesces. Effective wait
sets contain registered, live, non-socket Tool connections whose live selectors match
`session.started` exactly or by prefix. All other registrations are inert. Membership
committed after one session-init wait snapshot participates only in later snapshots.

A committed matching-current-session readiness acknowledgement removes its exact source
from the current wait set. Wrong-session, duplicate, unregistered, inert, or stale-source
acknowledgements do not release a waiter. Removing the last waiter may complete session
initialization and its existing derived work. Connected waiters have no deadline.

Readiness remains operational traffic and uses no declaration reservation. While global
initial activation remains pending, operational messages retain global arrival order.

## Skill projection

A skill candidate slot is (`ConnectionId`, `SkillName`). After commit, the harness
normalizes invocation flags, validates the skill name, samples the advertised file's
mtime, and truncates descriptions above the existing bound. Invalid candidates do not
enter state. Same-source/name publication replaces the existing slot. Across sources,
the greatest available mtime wins; first insertion remains selected on equal or
unavailable mtimes.

Invalid-name and truncation diagnostics remain harness-sourced mandatory replayable
notices. Collision diagnostics retain their separately defined live classification.
Diagnostics derive only from a committed declaration.

## AGENTS.md projection

An AGENTS.md slot is (`ConnectionId`, `PathBuf`). Same-source/path publication replaces
content without moving the slot. Other paths append in insertion order; equal paths from
different sources coexist. After each committed declaration, the harness injects the
complete post-replacement stack into every live agent as independently durable and
replayable `agent.user_message_injected` facts with the existing internal source `None`.
New-agent setup independently injects the then-current complete stack.

## Ordering and activation

The shell producer emits all skills, then all AGENTS.md declarations, then readiness on
one serialized writer. Tau permits one pending interception and queues later publications
FIFO, so every earlier declaration settles before readiness can commit and release the
barrier.

Registration, skill, and AGENTS.md are startup declaration families. Each admitted
pre-Ready publication reserves one frame and its encoded bytes under the existing
1024-frame/4-MiB activation bounds before interception. Pending declarations block
initial preflight and peer Ready activation. Replacement reaccounts the reservation;
registration's empty payload has equal-size valid replacements. Every committed skill
and AGENTS.md declaration stages in order; registration coalesces membership. Drop or
publisher disconnect releases reservations.

## Lifetime and failures

The four inputs, provider membership, skill candidates/winners, AGENTS.md slots, and wait
correlation are daemon-runtime-only. Disconnect removes the exact source's membership,
skills, files, activation state, and wait membership; skill winners recompute. Final
waiter disconnect may complete initialization. Session refresh removes and rebuilds only
effective waiting providers' discovery contributions; inert and unregistered sources
survive until replacement or disconnect. Respawn receives a new `ConnectionId` and must
re-register, retain a matching live selector, and republish discovery.

Publisher disconnect invalidates its generation and reservations. A publication parked
at another interceptor may later commit only as stale observation. Active-interceptor
disconnect preserves the existing pass behavior.

Required initial malformed/protocol/quota failures, pre-Ready timeout, and harness-owned
init/spawn/secret failures remain fatal. Optional initial failures disable the peer.
Clean EOF from a required non-Provider, non-socket extension remains compatibility
nonfatal. After global initial preflight, protocol failure isolates the connection.

## Persistence and replay

All four events default to `persist=false` and are unconditionally excluded from semantic agent,
session, and restore journals for either caller `Emit.persist` value. Raw callers keep
that value as generic publication metadata. First-party registration, skill, AGENTS.md,
and readiness sends use `persist=false`.

The raw events and runtime projections have no cold restore, historical replay, or
subscribe-time synthesis. Derived diagnostics and AGENTS injections keep their
independent persistence and replay classifications.

The configured-local-extension trust boundary is documented in [`SECURITY.md`](../SECURITY.md).
The per-agent context flow and all unrelated generic-emission rows remain outside this
specification.
