# DECISION-tau-ext-telegram-long-polling: Use Bot API long polling with reactive local coordination

Authority: confirmed, 2026-07-18, dpc

Telegram inbound delivery uses the Bot API `getUpdates` endpoint with long
polling. This protocol-provided pull mode keeps extension network activity as
outbound HTTP.

Local waits whose completion depends on Tau state, shutdown, or channel readiness
are notification-driven rather than periodic sleep-and-check loops. Explicit
timers are allowed for protocol heartbeat cadence, retry backoff, deadlines, and
pacing; prefer interruptible timed waits when shutdown or reconfiguration must wake
promptly.

Stream ownership is specified by
[`SPEC-tau-ext-telegram-stream-owner`](SPEC-tau-ext-telegram-stream-owner.md), and
gateway heartbeat and lease behavior by
[`SPEC-tau-telegram-gateway`](SPEC-tau-telegram-gateway.md).
