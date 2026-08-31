# SPEC-tau-provider-codex-cancellation: Transport cancellation and deadlines

## Record justification

Cancellation spans the synchronous turn owner, async WebSocket tasks, connection and pool waits, and standalone compaction, so no single local artifact can own the contract coherently.

ChatGPT/Codex Responses turns accept a typed local abort source that can both
report cancellation and register a wake callback. Ordered provider data uses a
one-event backpressured lane. Abort wakes and local writer failures use a
separate coalesced constant-size control path, which the synchronous turn owner
checks before queued provider data. Pool checkout and fresh connection waits
register the same abort source. Every abort wake rechecks that source; a stale
or coalesced hint does not impersonate cancellation.

Confirmed local cancellation returns typed `LlmError::Canceled`. Remote HTTP 499,
provider body text, and cancellation-looking prose remain remote retryable
failures and never impersonate local cancellation.

## Deadlines

The provider-stream watchdog is a five-minute idle deadline, not an absolute turn
duration or a polling cadence. It resets only after an upstream WebSocket frame.
Tau currently imposes no separate absolute ChatGPT/Codex turn deadline.

Fresh DNS, TCP, TLS, and WebSocket upgrade work has a separate 30-second
connection deadline. The provider-frame idle watchdog begins only after upgrade
and request send. Timeout produces a sanitized retryable transport failure.

Best-effort prewarm has the same 30-second connection deadline and a 30-second
absolute deadline for its non-generating response. It rechecks cancellation
before a successful socket returns to the pool.

## Pool and cleanup

A turn queued behind an active same-key WebSocket reservation waits until the
key clears or the registered abort wake changes the pool generation. A canceled
waiter returns typed cancellation and cannot send a stale request when the prior
turn releases.

Connection timeout or cancellation abandons the same-key reservation before
control returns to the scheduler. Profile or session invalidation removes cached
sockets and marks active reservations so a late owner cannot reinstall stale
state. Completed turns unregister wake callbacks. Prewarm completion likewise
cannot publish a socket after cancellation.

Standalone compaction uses the ordinary pooled WebSocket route. Its pool
reservation, fresh connection work, and provider-event wait register the same
abort source; cancellation abandons the reservation and returns typed
cancellation without publishing a late compacted result after the caller has
resumed. Retained unary compact helpers and fixtures are historical
compatibility evidence, not a production standalone-compaction path.
