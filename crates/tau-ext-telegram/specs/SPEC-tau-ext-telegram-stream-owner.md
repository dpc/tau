# SPEC-tau-ext-telegram-stream-owner: Telegram stream ownership

## Record justification

This contract spans registration and poller coordination in `src/lib.rs`,
advisory ownership and diagnostics in `src/stream_owner.rs`, and gateway stream
ownership, so no single implementation area can own it coherently.

Before polling or draining one API-base-plus-token update stream, Tau acquires an
exclusive advisory OS lock scoped to its shared state/extension root. The key is
a non-secret BLAKE3 fingerprint; metadata may contain process details, API base,
and fingerprint, never the token. Separate users, containers, and state roots are
outside this coordination scope. Local polling clones the owner lock into every
in-flight request so retirement cannot release ownership underneath an old
request. Gateway mode instead retains one owner lock for the gateway lifetime.

After locking, `getWebhookInfo` must report no webhook. Tau never removes a
webhook or drops its pending updates. HTTP 409 `getUpdates` contention stops
polling, clears active registrations, and produces a bounded diagnostic.
Diagnostics and lock metadata may expose the non-secret API base and
fingerprint, but never tokens, token-bearing URLs, or private text.

An in-progress registration reserves stream-owner interest while its
`getWebhookInfo` preflight runs without the state lock, but that reservation
alone never authorizes `getUpdates`. The poller retires stream ownership only
when there are neither registered agents nor pending registrations. Every
failed or stale registration completion releases its reservation and wakes
poller coordination.

The first lazy local poll drains backlog without long polling and publishes none
of the old updates. A stale-generation response cannot advance offset, mark the
backlog drained, or route work. Reconfiguration invalidates the local stream
generation and its registrations, offset, backlog state, and in-flight
responses.

Harness replay performs no Telegram I/O and reconstructs no live stream
ownership. Gateway restart recovery of its own durable cursor and deduplication
state is governed separately by
[SPEC-tau-telegram-gateway](SPEC-tau-telegram-gateway.md).
