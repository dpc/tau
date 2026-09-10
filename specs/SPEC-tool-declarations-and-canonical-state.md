# SPEC-tool-declarations-and-canonical-state: Tool declaration flow

## Record justification

Protocol declaration authority, client and extension publication, harness post-commit validation and registry state, and the [configured-peer security boundary](../SECURITY.md) jointly implement this lifecycle contract, so no one local artifact can coherently own its publisher provenance, canonical state, and process-lifetime rules.

## Scope

Authenticated configured Tool and Core extensions publish transient
`tool.registration_declared` and `tool.unregistration_declared` events. These
peer-owned declarations use ordinary generic `Emit` admission, interception,
commit, and broadcast. Configured Provider extensions may additionally declare
only tools with a nonempty `provider_scope` and withdraw their own declarations.
The scope must remain present after interception. Action, UI, socket, and
unconfigured peers have no declaration authority. No peer may author canonical `tool.register` or
`tool.unregister` state.

This specification covers registration lifecycle only. Tool requests, progress,
terminal reports, cancellation, action schemas, and later authority-matrix
families remain outside this slice.

## Provider-scoped backings

An optional exact provider namespace marks an ordinary tool backing, not a
hosted provider tool. It is eligible only when the selected model uses that
namespace and its serving connection owns the declaration. Role and capability
eligibility apply before selection. For an unmanaged public alias, one eligible
scoped backing precedes one eligible generic backing (fixed priorities 10 and
20); multiple eligible backings in either tier reject the surface even if the
other tier would win. There are no user-configured ordinary backing priorities.

Existing logical web candidate policy retains its named candidates, configured
priorities, and tie-breaks. Scoped ordinary web backings require an explicitly
listed `kind: tool` candidate and existing capability tags; they never satisfy
`kind: model_provider` or gain inferred web capabilities. Unlisted scoped web
backings are suppressed.

Prompt dispatch freezes selected internal identity and connection. A later
model switch recomputes a new prompt; connection replacement cannot redirect
an accepted call or retry it on a generic backing. Direct requests must name
the selected internal backing for the owning agent route. This is selection
fallback only, never execution retry or account failover.
Connection freezing includes a generic winner for an alias with a scoped
declaration. Unrelated generic-only aliases keep their existing reconnect
semantics.

## Downstream validation and canonical state

The harness processes a declaration only after it commits. It revalidates the
committed interception replacement against the captured connection and
configured-instance identity,
assigned `tool_prefix`, shared schema/example bounds, startup collision policy,
and unregistration ownership. A dropped declaration has no registry,
availability, or canonical-event effect. An invalid or non-owning committed
declaration produces a bounded harness diagnostic and no false canonical state.

An accepted registration or active withdrawal updates the runtime registry and
publishes a separate protected, transient, harness-authored `tool.register` or
`tool.unregister`. Canonical payloads carry the stable configured extension name
and harness-assigned logical configured-instance ID; that ID intentionally
survives supervised process respawn and is not a process-connection generation.
Delivery source is the harness.
Canonical events are immutable and must-pass through interception.

Pre-`Ready` declarations block activation until interception resolves. Their
committed payloads feed the existing deterministic startup staging and
preflight: last same-name registration wins, required extensions beat optional
ones, required-required conflicts fail startup, optional-optional conflicts
disable claimants, and invalid registrations claim no name. A pre-`Ready`
unregistration may cancel the source's own staged registration without exposing
an intermediate runtime tool.

## Lifetime and replay

Declarations and canonical tool state are process-lifetime runtime records.
They never enter agent/session semantic history and have no cold-restart replay
contract. A declaration deferred during extension activation still commits and
updates this process-global state when its captured connection and configured
instance remain current. Disconnect still removes the connection's registry
ownership and availability projections; it does not regenerate a peer declaration.

This implements the tool registration/unregistration rows of
[SPEC-peer-event-publication](SPEC-peer-event-publication.md).
