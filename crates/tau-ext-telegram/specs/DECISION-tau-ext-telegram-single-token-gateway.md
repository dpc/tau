# DECISION-tau-ext-telegram-single-token-gateway: Multi-session Telegram uses a single-token gateway

Authority: confirmed, 2026-07-22, dpc

## Decision

One standalone operator-supervised gateway owns each Telegram bot token and
update stream shared by multiple active Tau sessions. Lightweight per-session
gateway-client sidecars own only live harness registration and transient report
submission. Sidecars never poll Telegram or select raw chat IDs.

## Rationale

Telegram's cursor, webhook state, and conflict behavior are global to one Bot
API base plus token. One poller per Tau session would race that authority and
cannot provide coherent duplicate suppression or routing.

Component topology is
[ARCH-tau-telegram-gateway](ARCH-tau-telegram-gateway.md). Exact socket,
routing, lifecycle, persistence, and loss-window behavior is
[SPEC-tau-telegram-gateway](SPEC-tau-telegram-gateway.md).
