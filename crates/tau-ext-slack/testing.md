# Slack testing

Tests use a fake Slack client and loopback WebSocket surfaces; they require no
live credentials or sleeps. Production accepts WSS while loopback WS is
test-only. Event-driven tests cover bounded reservation/FIFO/ACK behavior,
reconnect, shutdown, framing, identity/install/route/config/session retirement,
message create/edit/reaction/delete publication, sent-before-result ordering,
replay without repost, retry/cancellation/writer failure, strict reaction and
mention arguments, ownership/ambiguity/deletion/capacity, and native-ID
nonexposure. Inject outcomes and clocks rather than waiting on wall time.

Ingress coverage includes reservation release, the hard 64-occurrence bound,
FIFO closure/drain, native duplicate suppression and exact target identity.
Delivery coverage fixes the absolute retry horizon, active-worker/ledger bounds,
fact-before-result ordering, cancellation, writer failure, and stable replay
without reposting or rewriting. Native IDs are rejected as route arguments, not
merely hidden.

Reaction coverage includes separate authorization, strict arguments, ownership
and idempotency, ambiguous effects, deletion, late completion, target/owner/
attempt capacity, and exact HTTP wire behavior. Mention coverage includes every
entity/code-span negative, leading-removal and normalization behavior, exact
register/unregister JSON, and literal `@slack_bridge` egress.
