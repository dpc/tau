# Slack testing

Tests use a fake Slack client and loopback WebSocket surfaces; they require no
live credentials or sleeps. Production accepts WSS while loopback WS is
test-only. Event-driven tests cover bounded reservation/FIFO/ACK behavior,
reconnect, stale-heartbeat expiration, shutdown, framing,
identity/install/route/config/session retirement, message
create/edit/reaction/delete report submission, report-before-result ordering, replay
without repost, retry/cancellation/writer failure, strict reaction and mention
arguments, ownership/ambiguity/deletion/capacity, and native-ID nonexposure.
Inject outcomes and use Tokio's paused clock rather than waiting on wall time.

Ingress coverage includes reservation release, the hard 64-occurrence bound,
FIFO closure/drain, native duplicate suppression, exact target identity,
responsive and stale peers, off-phase Pong deadlines, non-Pong traffic, and
shutdown/deadline interruption of blocked socket writes.
Delivery coverage fixes the absolute retry horizon, active-worker/ledger bounds,
report-before-result ordering, `persist=false` metadata, cancellation, writer failure, and stable replay
without reposting or rewriting. Native IDs are rejected as route arguments, not
merely hidden.

These tests own bridge-local admission and serialized report submission. Harness
tests own interception, canonical persistence, replay, projection, and wake.

Reaction coverage includes separate authorization, strict arguments, ownership
and idempotency, ambiguous effects, deletion, late completion, target/owner/
attempt capacity, and exact HTTP wire behavior. Mention coverage includes every
entity/code-span negative, leading-removal and normalization behavior, exact
register/unregister JSON, and literal `@slack_bridge` egress.
