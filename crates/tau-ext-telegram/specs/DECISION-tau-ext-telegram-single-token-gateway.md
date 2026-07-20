# DECISION-tau-ext-telegram-single-token-gateway: Multi-session Telegram uses a single-token gateway

Authority: unconfirmed

One Telegram bot token shared by multiple active Tau sessions uses one local gateway
owner for that Telegram update stream plus lightweight per-session gateway-client
sidecars. The gateway owns the token, `getUpdates` cursor, webhook/conflict handling,
stream lock, durable offset/deduplication state, allowlist and destination policy, and
Telegram sends. Sidecars retain only harness-local registration and direct
message-report submission and never poll Telegram or select raw chat ids.

## Rationale

Telegram's cursor, webhook state, and conflict behavior are global to one Bot API base
plus token. One poller per Tau session would race that authority and cannot provide
coherent duplicate suppression or routing.

## Downside

The gateway is another supervised component and does not provide durable
acknowledgement to sidecar delivery queues: exit after advancing the Telegram offset
can lose queued delivery. Legacy local polling remains valid for one session or
separate tokens.

Component topology is [ARCH-tau-telegram-gateway](ARCH-tau-telegram-gateway.md).
Exact socket, routing, lifecycle, persistence, and loss-window behavior is
[SPEC-tau-telegram-gateway](SPEC-tau-telegram-gateway.md).
