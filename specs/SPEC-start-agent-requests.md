# SPEC-start-agent-requests: Start-agent request processing

## Record justification

Start-agent requests span peer authority and interception, extension activation,
duplicate route rebinding, role and parent resolution, child persistence and
session placement, acceptance/result routing, and a first-party extension
producer. No component-local artifact describes the complete contract.

This specification implements the `agent.start_request` part of the request row
in
[DECISION-generic-peer-event-emission](DECISION-generic-peer-event-emission.md).

## Authority and commit

Every authenticated configured extension entry kind, including configured Core,
may author `agent.start_request` without a capability. Unconfigured, disconnected,
and socket peers have no authority. Harness-owned `agent_start` and peer
auto-start use direct internal methods and do not pass through peer admission.

Generic Emit captures the stable configured publisher, exact run-local
`ConnectionId`, instance id, kind, and current harness session before ordinary
same-name interception.
Drop produces no downstream effect. A replacement's complete committed payload
drives processing. After commit, the consumer revalidates the complete live
connection generation and session. A stale generation or session's request
remains observable but cannot accept, reject, rebind, mint, or start work for a
successor.

## Existing request semantics

Post-commit processing preserves the existing request contract:

- role selection validates an explicit role, defaults tool-backed requests to
  `engineer`, and otherwise uses the selected interactive role;
- an explicit parent must be loaded, and when a tool-call owner also supplies a
  parent they must agree;
- children inherit their resolved parent's session, ephemeral persistence mode,
  and inheritable metadata, but never copy the parent transcript;
- accepted work mints or reuses one agent id, publishes the shared
  harness-authored `agent.start_accepted`, sends the matching directed
  acceptance, and dispatches the instruction through the ordinary prompt path;
- validation failures send the existing requester-directed
  `agent.start_result` error without an accepted identity; and
- terminal results remain point-to-point to the bound requesting connection.

Duplicate identity is the stable configured extension name plus `query_id`.
Repeating an active or pending request reuses its existing agent id and rebinds
the directed acceptance/result route to the latest live connection. Existing
role validation still precedes duplicate lookup; duplicate payload fields do not
replace already admitted work.

## Activation, persistence, and producer

The request is globally ordered operational traffic, not an activation
declaration. A pre-Ready peer's complete raw Emit remains in the bounded
`DeferredExtensionMessage` queue. It is released only after Ready and global
activation, then undergoes ordinary interception, commit, and processing in wire
order.

Raw requests default transient and are unconditionally excluded from semantic
agent, session, and restore journals for either caller-supplied
`Emit.transient` value. They have no cold replay or current-state synthesis.
Canonical child lifecycle, membership, transcript, acceptance, and result
behavior retains its existing classification.

After a tool-backed worker completes, its durable child lifecycle remains
loaded and addressable independently of the completed request. Cold restore
preserves historical prompt/result provenance but does not reconstruct the
run-local request owner or result route. A fresh user turn is ordinary agent
work: its terminal response neither emits another `agent.start_result` nor
unloads the worker. The immutable `agent.started` parent supplies the restored
delegated navigation default. See
[DECISION-cold-restored-completed-worker-ownership](DECISION-cold-restored-completed-worker-ownership.md).

`tau-ext-std-notifications` explicitly emits transient requests for idle-summary
side agents and includes a random process-generation nonce in each monotonic
query-id sequence so respawned producers cannot rebind distinct work. The
configured-local-extension boundary is documented in
[`SECURITY.md`](../SECURITY.md). Unrelated authority rows remain outside this
specification.
