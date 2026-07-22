# SPEC-prompt-fragment-declarations-and-projection: Extension prompt-fragment declarations

## Record justification

Prompt-fragment publication spans the shared protocol, configured-extension
activation, generic interception and commit, harness prompt projection, and
extension startup helpers. No one component can document the complete ordering,
authority, lifetime, and persistence contract.

Every authenticated configured extension entry, including configured Core peers,
may publish `extension.prompt_fragment_publish` without a separate capability.
Unconfigured and socket peers have no authority; a claimed `Hello` kind grants
none. Harness-internal direct publication remains harness-owned rather than peer
admission.

Generic Emit captures the stable configured publisher and exact run-local
connection generation before interception.
`extension.prompt_fragment_publish` defaults to transient. Generic Emit preserves
any caller-supplied `Emit.persist` value as generic publication metadata,
applies ordinary same-name interception, commits and broadcasts the surviving
payload, then updates the prompt-fragment projection. Replacement cannot change
the publisher or event name. Drop performs no projection work. A declaration
from a disconnected or superseded connection may remain observable if it
committed, but cannot mutate the current generation's projection.

Pre-`Ready` declarations reserve one frame and its encoded bytes under the
extension activation limits before interception. A parked declaration blocks
activation. Replacement reaccounts its encoded size; drop or disconnect releases
the reservation. After commit, the declaration stages by fragment name, so the
last committed declaration for one source/name slot is activated with the
extension. Registration, prompt assembly, and slot replacement never occur in
Emit intake.

Declarations default to transient and never enter semantic session or agent
journals, regardless of the caller's `Emit.persist` value. They have no restore
processing, historical replay, or subscribe-time current-state synthesis. The
runtime projection survives session switches and is removed when its contributing
connection disconnects.

Prompt assembly orders ordinary source/name slots normally. As an existing
consumer-specific collision rule, only the lexicographically first
`shell.workdir` contributor in `BTreeMap<ConnectionId, …>` iteration order is
prompt-visible, selected before later priority/source/name sorting. Other
committed slots remain valid runtime declarations and can become visible after
disconnect or respawn changes the contributor set. The consumer omits this
shared fragment entirely when the turn's effective tool snapshot contains no
`shell:workdir` capability, so role/model/tool hiding cannot leave unusable
guidance behind.

This specification implements the prompt-fragment part of
[DECISION-generic-peer-event-emission](DECISION-generic-peer-event-emission.md)
under the configured-local-extension boundary in [`SECURITY.md`](../SECURITY.md).
