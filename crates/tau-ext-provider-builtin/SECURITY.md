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

## Prompt worker wakeups

Prompt workers communicate with the main manual runtime loop through a worker
message channel plus `ManualRuntimeWaker`. Every worker message must be enqueued
before calling `wake()`. Wakes are coalesced and do not identify which source is
ready, so the main loop must drain both harness input and worker messages before
blocking in `wait_for_wake()`. Regression tests should cover worker output that
wakes the loop before the worker sends its completion marker.

## Cancellation, EOF, and disconnect

Prompt cancellation is cooperative. Queued prompts can be removed immediately,
and active prompt retry sleeps can be aborted, but backend network reads are
owned by the backend transports. Harness input EOF should stop accepting new
input while allowing active prompt workers to finish and flush their messages.
Explicit disconnect/shutdown must abort retry sleeps and detach/finish without
leaving the harness waiting for a provider terminal path.
