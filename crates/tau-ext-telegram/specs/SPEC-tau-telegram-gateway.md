# SPEC-tau-telegram-gateway: Telegram single-token gateway

## Record justification

The single-token contract spans the standalone gateway's polling, persistence, routing, and socket service plus each extension sidecar's leases and harness report submission, so neither process can own it coherently.

One local gateway exclusively owns a shared token, `getUpdates` cursor,
webhook/conflict handling, stream lock, durable update checkpoints,
allowlist/destination policy, and sends. Per-session sidecars own live local
registration and canonical report confirmation; they never poll or select raw
chat IDs. The private bounded sanitized same-user socket is local coordination,
not a sandbox or authentication boundary, and token-bearing data stays
gateway-only.

Durable per-stream state contains the cursor, links/selections, recent IDs,
counters, and an ordered mixed checkpoint sequence. Each update is classified
once as `Routed(report_id)` with its exact sidecar delivery or `NonRouted`.
Routed checkpoints acknowledge only after the configured publisher's live
canonical `message.delivered` echo matches the report ID, target agent, and
message identity. Non-routed checkpoints acknowledge only after every required
Telegram reply and local mutation succeeds. The cursor advances and removes
only the contiguous acknowledged prefix across both classes.

The gateway persists a routed checkpoint before exposing it on the socket.
Socket responses select a bounded, non-destructive prefix for registrations
owned by that connection. Disconnect, unregister, lease expiry, and process
restart suppress delivery without deleting or retargeting the checkpoint;
re-registration replays its exact retained fields. An idempotent `ack_delivery`
request carries the report ID and frozen session/agent route. It accepts only an
exact matching pending record or recent acknowledgement, without requiring a
currently live lease, then commits canonical acknowledgement and contiguous cursor
advancement through the existing per-stream state-file transaction before returning
success. Poll progress is not required. The same transaction retains an
oldest-first bounded history of 128 validated report IDs and exact session/agent
routes, without report content, so the same route can retry after a committed
response is lost. A lost echo or uncommitted ACK replays the report, while a
committed ACK whose response is lost remains committed after reconnect or restart.
An echo that arrives after unregister, disconnect, or lease expiry can still retire
the frozen checkpoint; a mismatched route cannot mutate it.
State-save failures before rename restore the prior in-memory transaction.
Failure after rename but before successful parent-directory sync is
commit-unknown: the gateway poisons the shared state owner, refuses further
state operations, and requires restart rather than resuming from a divergent
claimed rollback.

Live leases disappear on unregister, goodbye, disconnect, heartbeat expiry, or
restart. A sidecar rechecks current registration before report submission.
Sends require a live registration owned by that exact requesting sidecar and a
gateway-selected configured or linked chat, never model coordinates.
Gateway-client sidecars keep the desired live route set separately from
connection-owned lease authority. One cancellable supervisor reconnects with
bounded low-rate backoff, sends a fresh `hello`, validates the gateway
generation, and exactly reannounces the current desired set before publishing
the new connection for delivery, ACK, or send. Disconnects and generation or
reannouncement hints retire only connection-owned authority; stale workers and
responses cannot publish deliveries, acknowledge reports, mutate newer
configuration, or restore removed routes.
An exact canonical echo validated while no gateway connection is live remains
a pending sidecar ACK obligation. The sidecar transmits it only after a
replacement connection completes hello validation and route reannouncement;
stale ACK responses cannot remove that obligation or publish deliveries.

Harness replay performs no Telegram I/O, report submission, or gateway ACK and
reconstructs no live sidecar registration or routing authority. Gateway restart
recovers its durable cursor, links/selections, recent IDs, and checkpoints; that
does not recreate a sidecar lease.

Protocol version is 0. Request frames are at most 8,192 bytes, response JSON
lines including their newline are at most 65,536 bytes, and socket errors are at
most 512 bytes. A successful operation exposes only the oldest pending delivery
prefix whose actual serialized response fits; later records remain for
subsequent responses, and selection never removes checkpoints. Heartbeat is 10
seconds, lease expiry 30 seconds, response delivery depth 32, outbound/reply
text 3,500 bytes, send rate 20 per 60 seconds, recent update dedup capacity 128,
and recent ACK retry capacity 128. Protocol errors close the connection; bounded
ordinary send failures keep it live.

Required Telegram replies, report submission, and canonical acknowledgement
cannot form one distributed transaction. Crashes or lost acknowledgements may
therefore duplicate replies and reports. `/start` and `/select` replace the same
durable state idempotently, but no remote effect is exactly once.

Long-poll stream ownership and local coordination are specified by
[SPEC-tau-ext-telegram-stream-owner](SPEC-tau-ext-telegram-stream-owner.md).
