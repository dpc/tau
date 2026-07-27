# SPEC-peer-event-publication: Peer event publication

## Status

Most peer event families use this boundary. Remaining `HarnessInputMessage::Emit`
intake paths still perform some event-specific work before ordinary publication;
they must transition to committed-event consumers or explicit point-to-point
messages.

## Record justification

Peer publication spans protocol authority and messages, client emission,
extension activation, harness admission and interception, semantic persistence,
broadcast, and many downstream domain consumers, so no one local artifact can
own the complete boundary.

`HarnessInputMessage::Emit` is only a peer request to publish its nested event.
The harness authenticates the peer, applies declarative authorship and bounded
resource admission, runs ordinary interception, commits with harness-owned
sequence and source metadata, and broadcasts the event. Generic intake must not
perform the requested domain operation, mutate semantic state, synthesize an
outcome, or select an event-specific publication path.

Committed live events form the boundary between publication and harness-owned
domain processing. Consumers may update projections, perform requested work, and
publish later canonical facts or outcomes. They cannot veto or rewrite the
triggering committed event and must not repeat live-only side effects during
replay.

Commit completes interception, runtime sequence and timestamp assignment, debug
record admission, any declaratively selected semantic append, and acceptance for
broadcast. Required semantic-store failure aborts commit. Event-family policy,
not generic intake, decides semantic persistence. `Emit.persist=false` requests
live-only publication; `true` requests ordinary eligible persistence but cannot
override an ineligible family. Replayed requests are observation only unless a
separate contract explicitly defines idempotent recovery.

The authenticated publisher is immutable metadata outside the rewriteable
event. Configured peers use their stable instance name. Persisted peer events
must retain a stable publisher identity; events without one remain live-only.
Interception may pass, drop, or replace an event with the same name and
publisher, subject to the same structural and authorship admission.

Peers may assert only facts they own. When the harness owns acceptance, routing,
canonical identity, or resulting state, the peer publishes a request,
declaration, offer, or report and the harness may later publish a distinct
canonical fact or outcome. Peer event authorship is default-deny and granted by
exact declarative policy to authenticated peer kinds and capabilities; a claimed
hello kind alone grants no authority.

Interactions that fundamentally require private or privileged data,
point-to-point response, synchronous correlation, or pre-publication handling
use dedicated input/output messages instead of overloading `Emit`. Those
messages remain outside event interception, subscriptions, and replay unless
their handlers deliberately publish separate events.

Extension activation may buffer emitted frames until peer and global startup
barriers allow publication. Buffering, ordering, authentication, and bounded
admission remain lifecycle concerns and do not authorize event-specific domain
processing in generic intake.
