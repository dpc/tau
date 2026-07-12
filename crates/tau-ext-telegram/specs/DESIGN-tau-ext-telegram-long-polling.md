# DESIGN-tau-ext-telegram-long-polling: Telegram updates use Bot API long polling

Status: confirmed, 2026-07-05, user

Telegram inbound delivery uses the Bot API `getUpdates` endpoint with long
polling. This is the protocol-provided pull delivery mode for Telegram bots and
fits this extension's architecture because it keeps all network activity as
outbound HTTP from the extension process.

This is distinct from local sleep-loop polling inside Tau. Local waits for Tau
state, shutdown, channels, timers, or other in-process conditions should be made
reactive instead of implemented as periodic sleep loops.
