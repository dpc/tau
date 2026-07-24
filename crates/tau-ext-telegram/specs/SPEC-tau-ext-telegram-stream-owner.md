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
webhook or drops its pending updates. HTTP 409 `getUpdates` contention produces a
bounded sanitized diagnostic of at most 1,024 bytes, stops polling, and clears
active registrations. Production requires HTTPS; loopback HTTP is test-only;
endpoint userinfo, query, and fragment are rejected. A non-secret API base may
appear in lock metadata and bounded diagnostics; tokens, token-bearing URLs, and
private text never do.

An in-progress registration reserves stream-owner interest while its
`getWebhookInfo` preflight runs without the state lock, but that reservation
alone never authorizes `getUpdates`. The poller retires stream ownership only
when there are neither registered agents nor pending registrations. Every
failed or stale registration completion releases its reservation and wakes
poller coordination.

The first lazy local poll drains backlog without long polling and publishes none
of the old updates. A stale-generation response cannot advance offset, mark the
backlog drained, or route work. Reconfigure invalidates registrations,
selections, links, offsets, and in-flight responses. Accepted original bodies
submit `message.delivered_reported`; successful sends submit
`message.sent_reported` before transient `tool.result_reported`, from which the
harness derives canonical facts.

Harness replay performs no Telegram I/O or report submission and reconstructs no live
registration, route, link, or stream ownership. This is distinct from gateway
restart recovery of its own durable cursor and deduplication state.

The inbound transport and local coordination choice is
[DECISION-tau-ext-telegram-long-polling](DECISION-tau-ext-telegram-long-polling.md).
