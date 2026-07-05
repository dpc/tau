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

## Telegram update streams are Tau-locked per state root

Status: unconfirmed

Telegram's Bot API `getUpdates` cursor is singleton state for one API base plus
bot token. Before this extension polls or drains that stream, Tau takes an
advisory exclusive OS lock scoped to the stream identity so another Tau process
sharing the same Tau state root fails closed instead of racing update offsets.

The lock key uses a non-secret BLAKE3 fingerprint over API base plus bot token.
Lock metadata may include owner process details, API base, and that fingerprint,
but never the raw bot token. The lock is advisory and local to processes that use
the same Tau state/ext root; separate users, containers, or explicitly separate
Tau state roots are outside this coordination scope.
