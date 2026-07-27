# SPEC-tau-telegram-gateway: Telegram single-token gateway

## Record justification

The single-token contract spans the standalone gateway's polling, persistence, routing, and socket service plus each extension sidecar's leases and harness report submission, so neither process can own it coherently.

One local gateway exclusively owns a shared token, `getUpdates` cursor,
webhook/conflict handling, stream lock, durable offset and recent-ID dedup state,
allowlist/destination policy, and sends. Per-session sidecars own only live local
registration and transient `message.delivered_reported` submission; they never poll or
select raw chat IDs. The private bounded sanitized same-user socket is local
coordination, not a sandbox or authentication boundary, and token-bearing data
stays gateway-only.

Durable per-stream state contains offset, links/selections, recent IDs, and
counters, never pending deliveries. Intentional handling or rejection advances
state, but required reply failure does not advance the offset or later same-batch
work. Enqueue counts as handled, so a crash after offset advance and before a
sidecar drains its live queue may lose report submission; there is no durable delivery
ACK or exactly-once guarantee.

Live leases disappear on unregister, goodbye, disconnect, heartbeat expiry, or
restart. Queued delivery drops when route/lifecycle authority disappears, and a
sidecar rechecks current registration before report submission. Sends require a live
registration owned by that exact requesting sidecar and a gateway-selected
configured or linked chat, never model coordinates.

Harness replay performs no Telegram I/O or report submission and reconstructs no
live sidecar registration or routing authority. Gateway restart may recover its
own durable offset, links/selections, and recent-ID state; that does not recreate
a sidecar lease.

Protocol version is 0. Request frames are at most 8,192 bytes, response JSON lines
including their newline are at most 65,536 bytes, and socket errors are at most
512 bytes. A successful operation removes and returns only the oldest queued
delivery prefix whose actual serialized response fits; later records remain for
subsequent requests. Enqueue rejects a record that cannot fit by itself with a
bounded content-free outcome. Removal still precedes the socket write, so write
failure may lose that selected prefix under the accepted non-durable queue
semantics. Heartbeat is 10 seconds, lease expiry 30 seconds, sidecar queue depth 32,
outbound/reply text 3,500 bytes, send rate 20 per 60 seconds, and recent dedup
capacity 128. Protocol errors close the connection; bounded ordinary send failures
keep it live.

Long-poll stream ownership and local coordination are specified by
[SPEC-tau-ext-telegram-stream-owner](SPEC-tau-ext-telegram-stream-owner.md).
