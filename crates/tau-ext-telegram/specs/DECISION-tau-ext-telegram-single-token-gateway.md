# DECISION-tau-ext-telegram-single-token-gateway: Multi-session Telegram uses a single-token gateway

Authority: unconfirmed

One Telegram bot token shared by multiple active Tau sessions uses one local gateway
owner for that Telegram update stream plus lightweight per-session gateway-client
sidecars. The gateway owns the token, `getUpdates` cursor, webhook/conflict handling,
stream lock, durable offset/deduplication state, allowlist and destination policy, and
Telegram sends. Sidecars retain only harness-local registration and direct
message-fact publication responsibilities and never poll Telegram or select raw
chat ids.

## Rationale

Telegram's cursor, webhook state, and conflict behavior are global to one Bot API base
plus token. One poller per Tau session would race that authority and cannot provide
coherent duplicate suppression or routing. A same-user local gateway centralizes it
while leaving each sidecar responsible for selecting a currently live local
registration and publishing `message.delivered`; the harness stamps, commits,
and projects the fact through its generic post-commit path. Gateway traffic does
not masquerade as agent-to-agent messaging.

## Tradeoffs and constraints

Gateway registrations and sidecar delivery queues are bounded live leases, while
Telegram offset and duplicate-suppression state are durable, but a gateway exit after
advancing the offset but before a sidecar drains its live queue can therefore
lose that queued delivery before the sidecar publishes a message fact; the
design does not claim a durable delivery acknowledgement. The socket and
labels are private, bounded, and sanitized, token-bearing data remains gateway-only, and
the model never supplies a Telegram destination. Legacy local-poll mode remains valid
for a single session or separate tokens.

Component topology is [ARCH-tau-telegram-gateway](ARCH-tau-telegram-gateway.md).
Exact socket, routing, lifecycle, persistence, and loss-window behavior is
[SPEC-tau-telegram-gateway](SPEC-tau-telegram-gateway.md).
