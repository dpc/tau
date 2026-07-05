# Design decisions

This file records major design decisions for the ChatGPT/Codex provider
transport crate.

## WebSocket cancellation is a cooperative wake source

Status: inferred

ChatGPT/Codex WebSocket turns run from synchronous prompt workers while socket IO
is owned by background Tokio reader/writer tasks. A turn waiting for upstream
events registers a `TurnAbortWaker`; cancellation sends an `AbortWake` hint into
the same inbound event queue as transport events. The turn then re-checks
`TurnAbort::is_aborted()` and returns the standard 499 harness cancellation error
only when the abort source confirms cancellation.

This keeps cancellation prompt without shortening the provider-event timeout.
The 120 second timeout still means "no upstream events arrived for this long",
not "wake periodically to poll cancellation".

The shared WebSocket pool uses the same cancellation seam when a prompt turn is
queued behind an active same-key reservation. Checkout waits on the pool
condition variable until the busy key clears or a registered abort waker bumps
the pool's abort-wake generation. A canceled waiter must return the standard 499
path instead of starting a stale network turn after the earlier same-key turn
releases.

## Backend transport behavior is covered by focused local tests

Status: inferred

This crate owns ChatGPT/Codex HTTP/SSE parsing, WebSocket turn and pool
behavior, transport selection, the no-HTTP-fallback policy for
WebSocket-capable configs, provider-cache key derivation, and provider-specific
retry/error mapping. Backend transport behavior should be tested here with
focused unit tests and local fakes rather than duplicated in
`tau-ext-provider-builtin`.

For models whose resolved config advertises WebSocket support, that support is a
routing commitment rather than a speculative optimization. WebSocket
capability/limit failures and exhausted retryable WS failures must surface as
provider errors for the turn instead of silently retrying the same turn over
HTTP/SSE.

WebSocket changes should cover observable turn/pool contracts such as pool-key
identity, reservation/release behavior, reconnect behavior, provider-event
timeouts, cancellation returning the standard 499 harness path, and abort wakers
waking blocked turn waits without relying on short receive polling. Parser and
streaming changes should keep using focused event/delta/snapshot regression
tests, with broader provider response streaming guidance in `../../docs/testing.md`.

## Responses replay sidecars are syntax, not semantics

Status: unconfirmed

Responses full-transcript replay preserves provider-visible syntax sidecars for
fields Tau does not semantically model, including raw tool-call argument JSON,
tool-call item envelopes, opaque reasoning/compaction items, and raw assistant
`message` items. These sidecars protect provider cache identity and replay
continuity when a turn cannot rely on `previous_response_id`.

Typed Tau fields remain authoritative. Tool routing uses parsed `ToolCallItem`
fields, assistant message replay rebases text and phase from `MessageItem`, and
raw assistant message sidecars are used only after validating that they are
Responses assistant `message` items.
