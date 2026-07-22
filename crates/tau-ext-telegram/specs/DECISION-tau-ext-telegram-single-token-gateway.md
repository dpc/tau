# DECISION-tau-ext-telegram-single-token-gateway: Multi-session Telegram uses a single-token gateway

Authority: confirmed, 2026-07-22, dpc

One standalone operator-supervised gateway owns each Telegram bot token and update
stream shared by multiple active Tau sessions. Lightweight per-session gateway-client
sidecars own only live harness registration and transient message-report submission.
The gateway owns the token, `getUpdates` cursor, webhook/conflict handling, stream
lock, durable offset/deduplication state, allowlist and destination policy, and
Telegram sends. Sidecars never poll Telegram or select raw chat ids.

This confirmation covers that topology and ownership split, including acceptance of
the documented non-durable sidecar-queue loss window. It does not claim that every
linked specification detail already conforms.

## Rationale

Telegram's cursor, webhook state, and conflict behavior are global to one Bot API base
plus token. One poller per Tau session would race that authority and cannot provide
coherent duplicate suppression or routing.

## Downside

The gateway is a standalone component that operators must supervise and does not
provide durable acknowledgement to sidecar delivery queues: exit after advancing the
Telegram offset can lose queued delivery. Legacy local polling remains valid for one
session or separate tokens.

Component topology is [ARCH-tau-telegram-gateway](ARCH-tau-telegram-gateway.md).
Exact socket, routing, lifecycle, persistence, and loss-window behavior is
[SPEC-tau-telegram-gateway](SPEC-tau-telegram-gateway.md).
