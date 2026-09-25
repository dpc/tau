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
The publishing extension must propagate failure of any mandatory correlated
snapshot, prerequisite metadata, context, or readiness write to its connection
lifecycle so disconnect cleanup can release the wait.

All raw events default to `persist=false`, remain excluded from semantic journals,
and have no cold or historical replay. The durable initialization replacement and
transient current projection are specified by
[SPEC-session-discovery-declarations-and-readiness](SPEC-session-discovery-declarations-and-readiness.md).

The local configured-extension trust boundary and bounded-wait risk are documented
in [`SECURITY.md`](../SECURITY.md).

## Source-local workdir replacement

A source may bind its initial discovery to its instance metadata key and supply
retained user-only fallback inputs, complete user candidate identities, and an
opaque source-owned capture of the original user scope. Eligibility of every
supplied source candidate and captured user candidate is sampled at acceptance,
including global collision losers; another source's refresh cannot resample it.
Only the moving source's project eligibility is refreshed; later invocation still
reads live skill bodies. A canonical mutation of that key starts a
new discovery revision under the same load identity. The protected transient
`harness.agent_discovery_refresh_requested` carries the actual committed value
and source-local token, not the setter's requested path. Only a current-generation,
matching-token reply settles that source; ordinary `extension.context_ready`
cannot bypass an active refresh. Other sources may refresh concurrently without
superseding each other.

The current revision installs through the durable initialization replacement.
Only its committed protected projection acknowledges correlated setters and
releases consumer readiness. Superseded setters receive explicit supersession
outcomes rather than retaining their reservations indefinitely. A scan deadline
or source disconnect settles with the retained user-only fallback and degraded
diagnostic. Supervisor replacement rebinds only the replaced configured source's
retained ownership, preserving the load identity and enabling later repair.
Refresh requests carry the original user capture so replacement scanners restore
its exact ordering, metadata, paths and instructions without rereading user
files. Unsolicited replay snapshots cannot replace an accepted same-load binding.
An invalid restored capture degrades to the retained user fallback, never an
ambient recapture.

Definite persistence-capacity rejection retains only the exact already-intercepted
initialization replacement. Capacity recovery retries that fact without another
interception, only while its load and revision remain current. Supersession
discards it; hard or ambiguous persistence failures neither acknowledge
installation nor authorize this retry.

A pending user-skill expansion may defer a FIFO steer fold. Deferral transfers
the exact optional publication completion rather than reporting an empty fold;
queued prompts remain in the normal cancellable FIFO. Installation resumes that
fold before dispatch wakes and holds its ownership barrier through synchronous
preprocessing and completion callbacks. If expansion removes every queued item,
the original empty-fold completion still runs once. Content-independent internal
steering does not itself require a current discovery catalog.

An undelivered prompt may retain its exact live checkpoint callback or full
one-shot phase outside the publication queue while discovery is pending. A
retained live checkpoint is not replay/disconnect uncertainty and cannot be
superseded merely because another activating UI input arrives during that wait.
Installation resumes that same owner, rebuilding only the current synthetic
bootstrap and system discovery content from its captured capability surface.
It never re-prepares accounting, changes prompt/model/compaction identity,
duplicates `agent.prompt_started`, recreates replayed work, or resends a delivered
request. This refines
[SPEC-provider-prompt-materialization-authority](SPEC-provider-prompt-materialization-authority.md).
