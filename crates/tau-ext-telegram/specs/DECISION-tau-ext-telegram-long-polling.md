# DECISION-tau-ext-telegram-long-polling: Use Bot API long polling with reactive local coordination

Authority: confirmed, 2026-07-18, dpc

Telegram inbound delivery uses the Bot API `getUpdates` endpoint with long
polling. This protocol-provided pull mode keeps extension network activity as
outbound HTTP rather than requiring a public webhook endpoint. It accepts a
long-lived poller and Telegram cursor ownership as the cost of simpler deployment.

Stream ownership is specified by
[`SPEC-tau-ext-telegram-stream-owner`](SPEC-tau-ext-telegram-stream-owner.md).
