# SPEC-tau-ext-telegram-stream-owner: Telegram stream ownership

## Record justification

This contract spans registration and poller coordination in `src/lib.rs`,
ordered local checkpoints in `src/live_checkpoint.rs`, advisory ownership and
diagnostics in `src/stream_owner.rs`, bounded pending timing in
`src/pending_retry_backoff.rs`, and gateway stream ownership, so no single
implementation area can own it coherently.

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
of the old updates. After that drain, every observed update is classified as
routed or non-routed in one ordered in-memory checkpoint sequence. Routed
updates retain their exact report and acknowledge only when a canonical
`message.delivered` fact matches the configured publisher, target agent,
message identity, and private report ID. A missing echo leaves the cursor in
place so Telegram redelivery replays the retained routed report without
recomputing routing. Non-routed updates emit no Tau event and acknowledge when
processing returns; their Telegram replies are best effort and may repeat when
an earlier routed checkpoint blocks the cursor. `/start` and `/select` replay
replace the same local link or selection idempotently. The update offset
advances only through the contiguous acknowledged prefix across both classes;
none of these rules promises exactly-once remote effects.

A stale-generation response cannot advance offset, mark the backlog drained, or
route work. Reconfiguration invalidates the local stream generation and
in-flight responses. Changing the API-base-plus-token stream identity also
clears its offset, checkpoints, and backlog state. A same-stream configuration
generation retains already submitted checkpoints so their exact canonical
echoes or Telegram redelivery can still retire them.

Harness replay performs no Telegram I/O and reconstructs no live stream
ownership. Gateway restart recovery of its own durable cursor and deduplication
state is governed separately by
[SPEC-tau-telegram-gateway](SPEC-tau-telegram-gateway.md).
