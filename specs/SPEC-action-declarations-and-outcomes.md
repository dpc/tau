# SPEC-action-declarations-and-outcomes: Action declarations and outcomes

## Record justification

Action authority and lifecycle span the handshake capability, generic peer publication, live registry, private invocation routing, extension producers, and UI consumers, so no single implementation artifact can own the complete contract.

Configured Provider, Tool, Action, and Core peers may declare `PeerCapability::ActionProvider`; kind alone grants no Action authority. An authenticated live configured peer with that capability may publish transient `action.schema_declared`, `action.result_reported`, and `action.error_reported` observations through ordinary generic Emit admission, interception, commit, and broadcast. The harness revalidates the immutable configured name, logical instance, exact connection generation, and capability after commit before applying any semantic effect.

A schema declaration is a complete atomic snapshot. The latest valid snapshot replaces that logical owner's current schema, an empty snapshot withdraws it, an invalid replacement preserves the prior snapshot, and an identical snapshot has no canonical effect. The harness publishes protected transient `action.schema_published` current state stamped with `(configured extension name, logical instance id)`. Late subscribers reconstruct this state from the live registry; disconnect removes it, and a respawn must redeclare it. Dynamic root selection remains built-in-first, then lexicographically lowest logical owner, as refined by [SPEC-tau-cli-action-completions](../crates/tau-cli/specs/SPEC-tau-cli-action-completions.md).

`action.invoke` remains a private transient UI-to-owner request and must not be broadcast, persisted, journaled, or exposed through unsafe debug paths. Routing validates the current owner-qualified schema and captures the exact configured name, logical instance, connection generation, action id, session, requester, and globally unique pending invocation id only after directed delivery succeeds.

Terminal reports commit before correlation. The first report matching every captured owner, generation, action, invocation, session, and requester invariant consumes the pending call and produces protected harness-authored `action.result` or `action.error` only for the original requester. Unknown, duplicate, late, wrong-owner, wrong-action, stale-generation, and post-disconnect reports remain peer observations without canonical effect. Provider disconnect fails its pending requests once; requester disconnect drops them; neither schema nor pending work transfers across respawn.

All Action declarations, reports, canonical state, and canonical outcomes remain process-runtime transient. They do not enter agent or session semantic journals and gain no cold-restart authority. This specification implements the Action family of [SPEC-peer-event-publication](SPEC-peer-event-publication.md).
