# tau-provider-chatgpt security notes

This crate sends prompt/tool context to ChatGPT/Codex endpoints and parses
provider responses back into Tau stream state. Treat upstream responses and
diagnostics as crossing an external-provider trust boundary.

## WebSocket cancellation

WebSocket cancellation is cooperative. `TurnAbortWaker` wakes the synchronous
turn loop so it can observe the caller's authoritative `is_aborted()` state and
return the normal 499 harness cancellation result. `InboundEvent::AbortWake` is
not itself proof of cancellation; it must remain a wake hint only, because stale
or delayed hints can arrive on pooled connections after a previous turn's guard
has been dropped.

The transport must preserve the 120 second no-provider-event timeout separately
from cancellation wakeups. Do not replace abort wakers with periodic short
timeouts that hide idle sockets or make cancellation latency depend on polling.
