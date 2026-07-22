# SPEC-tool-declarations-and-canonical-state: Tool declaration flow

## Scope

Authenticated configured Tool and Core extensions publish transient
`tool.registration_declared` and `tool.unregistration_declared` events. These
peer-owned declarations use ordinary generic `Emit` admission, interception,
commit, and broadcast. Provider, Action, UI, socket, and unconfigured peers have
no declaration authority. No peer may author canonical `tool.register` or
`tool.unregister` state.

This specification covers registration lifecycle only. Tool requests, progress,
terminal reports, cancellation, action schemas, and later authority-matrix
families remain outside this slice.

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
contract. A declaration deferred across session rollover still commits and
updates this process-global state when its captured connection and configured
instance remain current. Disconnect still removes the connection's registry
ownership and availability projections; it does not regenerate a peer declaration.

This implements the tool registration/unregistration rows of
[DECISION-generic-peer-event-emission](DECISION-generic-peer-event-emission.md).
