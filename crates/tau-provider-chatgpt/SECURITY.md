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

The transport must preserve the five-minute no-provider-event idle timeout
separately from cancellation wakeups. That watchdog resets on each SSE `data:`
event or WebSocket frame, ignores SSE comments/heartbeats and partial-line byte
trickles, and is not a separate absolute turn-duration cap. Do not replace abort
wakers with periodic short timeouts that hide idle sockets or make cancellation
latency depend on polling. Pool checkout cancellation uses the same wake
discipline: an abort wake only causes checkout to re-check authoritative abort
state, and a canceled same-key waiter must not send a delayed stale request
after the prior reservation clears.

## WebSocket downgrade prevention

For ChatGPT/Codex configs that advertise WebSocket support, the WebSocket
transport is a routing commitment. WebSocket capability/limit failures and
retryable WebSocket failures after bounded retry exhaustion must surface as
provider errors instead of silently replaying the prompt over HTTP/SSE. This
keeps transport behavior visible, avoids surprising downgrade paths, and
prevents masking a WebSocket-specific upstream or pool failure as an unrelated
HTTP/SSE turn.

## Raw Responses replay sidecars

Raw provider replay sidecars are external-provider-authored transcript data. They
may contain provider ids, status fields, annotations, encrypted reasoning blobs,
or future fields that Tau does not understand. Treat them as sensitive
provider-visible syntax for replay/cache fidelity, not as semantic authority.

Replay code must validate the sidecar item kind before reusing raw JSON and must
rebase controlled semantic fields from typed Tau structures. In particular,
assistant `message` sidecars may only replay as assistant messages, and their
model-visible text/phase must come from `MessageItem` rather than from an
unchecked raw blob.

## Streaming progress metadata

Provider response progress for streamed assistant text, reasoning text, and
tool-call/custom-tool input must stay content-free. Emit byte counters,
sample-window durations, output indices, and bounded labels only; never copy raw
text/reasoning/argument/input bytes into progress status.
