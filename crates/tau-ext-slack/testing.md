# Slack testing

Tests use a fake Slack client and loopback WebSocket surfaces; they require no
live credentials or sleeps. Production accepts WSS while loopback WS is
test-only. Event-driven tests cover bounded reservation/FIFO/ACK behavior,
reconnect, stale-heartbeat expiration, shutdown, framing,
identity/install/route/config/session retirement, message
create/edit/reaction/delete report submission, report-before-result ordering, replay
without repost, ingress report replay before canonical confirmation,
post-confirmation duplicate suppression, immediate delete revocation, retained
admission capacity, new/replayed report serialization against disconnect,
shutdown, unload, and fatal writer retirement, retry/cancellation/writer failure,
strict reaction and mention
arguments, ownership/ambiguity/deletion/capacity, and native-ID nonexposure.
Inject outcomes and use Tokio's paused clock rather than waiting on wall time.

Ingress coverage includes reservation release, the hard 64-occurrence bound,
FIFO closure/drain, native duplicate suppression, exact target identity,
exact canonical event/agent/publisher/message/report correlation, responsive and
stale peers, off-phase Pong deadlines, non-Pong traffic, and
shutdown/deadline interruption of blocked socket writes.
Delivery coverage fixes the absolute retry horizon, active-worker/ledger bounds,
report-before-result ordering, `persist=false` metadata, cancellation, writer failure, and stable replay
without reposting or rewriting. It also covers exact canonical sent-fact
correlation, echoes racing typed-result submission, pending replay, and
post-confirmation authority installation. Native IDs are rejected as route
arguments, not merely hidden.

These tests own bridge-local admission, serialized report submission, canonical
echo correlation, pending-ledger transitions, and local authority installation.
Harness tests own interception, canonical persistence, replay, projection,
broadcast, and sink-delivery mechanics.

Reaction coverage includes separate authorization, strict arguments, ownership
and idempotency, ambiguous effects, deletion, late completion, target/owner/
attempt capacity, and exact HTTP wire behavior. Mention coverage includes every
entity/code-span negative, leading-removal and normalization behavior, exact
register/unregister JSON, and literal `@slack_bridge` egress.
