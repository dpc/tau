# SPEC-tau-provider-chatgpt-cancellation: Transport cancellation and deadlines

ChatGPT/Codex Responses turns accept a typed local abort source that can both
report cancellation and register a wake callback. WebSocket turn waits enqueue
an abort-wake hint through the same inbound queue as transport events. Pool
checkout and fresh connection waits register the same source. Every wake path
rechecks the abort source; stale or coalesced hints do not cancel a turn.

Confirmed local cancellation returns typed `LlmError::Canceled`. Remote HTTP 499,
provider body text, and cancellation-looking prose remain remote retryable
failures and never impersonate local cancellation.

## Deadlines

The provider-stream watchdog is a five-minute idle deadline, not an absolute turn
duration or a polling cadence. It resets only after an upstream SSE `data:` event
or WebSocket frame. SSE comments, heartbeats, and partial-line byte trickles do
not reset it. Tau currently imposes no separate absolute ChatGPT/Codex turn
deadline.

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

This behavior implements
[DECISION-tau-provider-chatgpt-cooperative-cancellation](DECISION-tau-provider-chatgpt-cooperative-cancellation.md).
