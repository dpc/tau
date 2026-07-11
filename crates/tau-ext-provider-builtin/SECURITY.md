# tau-ext-provider-builtin security notes

This crate is Tau's built-in provider bridge. It handles local provider
credentials, receives model-visible prompt/tool context from the harness, sends
requests to external model services, and turns provider responses back into Tau
protocol events.

## Credentials and diagnostics

Provider profile files and OAuth tokens are local secrets. Do not echo access
tokens, refresh tokens, API keys, pasted redirect URLs, authorization codes, or
PKCE verifiers to model-visible output, notices, traces, debug logs, or test
fixtures. Debug request/response capture may include full prompt contents and
tool results, so it must remain gated on explicit durable-session policy from
`harness.session_dir`.

## Provider response trust boundary

External provider responses are untrusted prompt-surface data. Keep emitted
provider events bounded and deterministic, and treat provider diagnostics as
model-visible content unless they are kept entirely inside private debug
captures.

Streamed assistant text, reasoning text, and tool-call/custom-tool input cross
the same external-provider boundary. Never copy raw streamed
text/reasoning/argument/input bytes into status text, notices, traces, or final
transcript rendering. Provider response stats are public, content-free metadata on transient
`provider.response_updated` events: providers own the prompt-local byte counter,
may emit the first non-empty previous/current sample promptly, emit later
non-terminal samples at no more than 1Hz, and may emit a final flush. The harness
validates ownership and broadcasts the stats unchanged.

## Prompt worker wakeups

Prompt workers communicate with the main manual runtime loop through a worker
message channel plus `ManualRuntimeWaker`. Every worker message must be enqueued
before calling `wake()`. Wakes are coalesced and do not identify which source is
ready, so the main loop must drain both harness input and worker messages before
blocking in `wait_for_wake()`. Regression tests should cover worker output that
wakes the loop before the worker sends its completion marker.

Delayed retries use one scheduler thread and never retain a worker permit.
Cooldown keys contain only configured provider namespaces, never account ids,
tokens, headers, prompts, or response bodies. Retry status uses normalized,
bounded provider-independent reasons rather than provider-authored prose.

## Cancellation, EOF, and disconnect

Prompt cancellation is cooperative. Queued prompts can be removed immediately,
active prompt retry sleeps can be aborted, and backend transports may register
per-prompt abort wakers to wake their own blocking waits. ChatGPT/Codex
WebSocket turns use that waker to leave an idle provider-event receive and
return the normal canceled terminal path; other backend network reads remain
transport-owned and must not be treated as hard-interrupted unless their backend
documents such a wake path. Harness input EOF should stop accepting new input
while allowing active prompt workers to finish and flush their messages. Explicit
disconnect/shutdown must abort retry sleeps, wake registered backend abort
wakers, and detach/finish without leaving the harness waiting for a provider
terminal path.

## Terminal provider failures

Adapters, not display text, classify canonical provider rejection envelopes. The built-in
scheduler bypasses all retry and delay paths for typed terminal failures, including context-window
rejection, even when required-work retries are configured without a practical limit. Raw provider
bodies never become the typed category.
