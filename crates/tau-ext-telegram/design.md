# Design decisions

This file records major design decisions currently embodied by this directory's code, and how authoritative each decision is. It is not an architecture overview, ADR log, todo list, roadmap, implementation guide, or changelog.

## Telegram updates use Bot API long polling

Status: confirmed, 2026-07-05, user

Telegram inbound delivery uses the Bot API `getUpdates` endpoint with long
polling. This is the protocol-provided pull delivery mode for Telegram bots and
fits this extension's architecture because it keeps all network activity as
outbound HTTP from the extension process.

This is distinct from local sleep-loop polling inside Tau. Local waits for Tau
state, shutdown, channels, timers, or other in-process conditions should be made
reactive instead of implemented as periodic sleep loops.
