# ARCH-tau-ext-provider-builtin: tau-ext-provider-builtin architecture

For ChatGPT account quota, the main runtime loop owns opaque profile epochs,
strict sequences, bounded full/sparse reconciliation, and one coalesced full
fetch per profile. Prompt workers report normalized rolling observations
through the existing enqueue-before-wake worker channel; quota failure never
delays inference or consumes prompt retry budget. This implements
[DECISION-provider-quota-pacing](../../../specs/DECISION-provider-quota-pacing.md).

Provider output is constrained by [SPEC-provider-response-streaming](../../../specs/SPEC-provider-response-streaming.md).

The provider boundary is not trusted to request transcript mutation or
compaction. Built-in providers may report a typed context-window failure, but
the harness clears provider-supplied recovery disposition and independently
checks operation, accepted output, checkpoint, model capability, role policy,
and branch correlation.

This crate is Tau's built-in provider bridge. It handles local provider
credentials, receives model-visible prompt/tool context from the harness, sends
requests to external model services, and turns provider responses back into Tau
protocol events.

## Profile and model ownership

Tau state `auth.d/<namespace>.json` files define built-in provider namespaces;
the serialized profile kind selects the backend family. ChatGPT profiles publish
the model matrix owned by `tau-provider-codex`, Chat Completions profiles
publish their configured models, and OpenRouter profiles publish configured or
fetched models through Chat Completions configuration. Prompt dispatch resolves
an exact configured model in the selected namespace. Missing or invalid mutable
profile, model, or auth state remains visibly pending and is resolved again for
later attempts.

The extension also owns the serialized Chat Completions and OpenRouter DTOs,
OpenRouter discovery/cache behavior, configured-model publication, and conversion
to the Chat Completions backend's non-serialized finite-attempt inputs. The
backend returns typed success, retry, cancellation, terminal failure, and
semantic-progress facts; it cannot serialize harness frames.

ChatGPT profiles capture Responses mode at process startup. Model publication,
prompt, prewarm, retry, and quota resolution share that captured value. Mutable
credential reload and OAuth refresh preserve it; an on-disk mode edit takes
effect after restart. Different namespaces may select different modes.

This ownership implements
[DECISION-tau-ext-provider-builtin-profile-ownership](DECISION-tau-ext-provider-builtin-profile-ownership.md)
and the selected surface is constrained by
[DECISION-tau-provider-codex-responses-surface-selection](../../tau-provider-codex/specs/DECISION-tau-provider-codex-responses-surface-selection.md).

## Credentials and diagnostics

Provider profile files and OAuth tokens are local secrets. Do not echo access
tokens, refresh tokens, API keys, pasted redirect URLs, authorization codes, or
PKCE verifiers to model-visible output, notices, traces, debug logs, or test
fixtures. Debug request/response capture may include full prompt contents and
tool results, so it must remain gated on explicit durable-session policy from
`harness.session_dir`.

Permanent OAuth refresh rejection is process-local main-loop state keyed by the
provider namespace and exact credential plus startup-selected Responses mode.
Startup quota, scheduled quota, prewarm, prompt, and retry-time resolution share
that cache. Resolution reloads under the auth-file lock; the locked generation
is authoritative for both rejection caching and any still-valid access-token
fallback. Only closed credential-invalidating codes on HTTP 400/401 suppress a
repeat. Expired access tokens never fall back, while preemptive refresh may use
the authoritative bearer until its exact expiry. Credential/profile change
invalidates suppression; cold process restart may make one new attempt. This
does not change the logical prompt scheduler's existing slow Auth cadence.

## Provider response trust boundary

External provider responses are untrusted prompt-surface data. Keep emitted
provider events bounded and deterministic, and treat provider diagnostics as
model-visible content unless they are kept entirely inside private debug
captures.

Streamed assistant text, reasoning text, and tool-call/custom-tool input cross
the same external-provider boundary. Never copy raw streamed
text/reasoning/argument/input bytes into status text, notices, traces, or final
transcript rendering. Provider response stats are public, content-free metadata
on transient `provider.response_updated` events: backends own prompt-local byte
counters and the extension owns public sampling and writes. It may emit the first
non-empty previous/current sample promptly, emits later non-terminal samples at
no more than 1Hz, and may emit a final flush. The harness validates ownership and
broadcasts these stats unchanged.

## Prompt worker wakeups

Protocol startup publishes provider client kind, exact subscriptions, the
startup `ProviderModelsUpdated` snapshot, and then `Ready`. Current-state session
directory restore is the only replay catch-up used by this provider. Prewarm,
session-directory, cancel, and shutdown inputs use exact selectors; directed
`ui.retry_prompt` and `agent.prompt_created` arrive as routed live deliveries
without subscribing to or replaying provider work.

Prompt workers return typed provider frames and completion notices to the main
loop. The normal `tau-client` writer remains the only serialization path; workers
never write protocol frames directly.

Prompt workers communicate with the main manual runtime loop through a worker
message channel plus `ManualRuntimeWaker`. Every worker message must be enqueued
before calling `wake()`. Wakes are coalesced and do not identify which source is
ready, so the main loop must drain both harness input and worker messages before
blocking in `wait_for_wake()`. Regression tests should cover worker output that
wakes the loop before the worker sends its completion marker.

Prewarm workers use the same enqueue-before-wake completion channel but are
supervised separately from prompt concurrency. Duplicate cache-owner work is
suppressed, cancellation remains owned until exact completion, and main-loop
cancel/shutdown/profile transitions wake transport waits without awaiting them
inline.
On terminal harness disconnect the loop registry is dropped after cancellation;
the detached worker retains its cancellation source and finite network bounds,
so exact completion is no longer reconciled into a loop that has ceased to
exist.

Delayed retries use one scheduler thread and never retain a worker permit.
Cooldown keys contain only configured provider namespaces, never account ids,
tokens, headers, prompts, or response bodies. Retry status uses normalized,
bounded provider-independent reasons rather than provider-authored prose.
The main runtime loop generates shared cooldown evidence and attaches an exact
generation only to a manually admitted finite probe attempt. A validated
successful terminal may remove that generation; the scheduler then removes only
its matching constraints, preserving prompt-local deadlines and ownership while
adding stable release jitter. Inference profile identities cover every backend
family and rotation invalidates old-profile cooldowns. Quota acquisition and
display telemetry never own or mutate inference cooldowns.

## Cancellation, EOF, and disconnect

Prompt cancellation is cooperative. Queued prompts can be removed immediately,
active prompt retry sleeps can be aborted, and backend transports may register
per-prompt abort wakers to wake their own blocking waits. ChatGPT/Codex
WebSocket turns use that waker to leave an idle provider-event receive and return
the normal canceled terminal path. Targeted cancellation wakes the matching
registered backend; broadcast cancellation and disconnect/shutdown wake all
registered backends. Other backend network reads remain transport-owned and must
not be treated as hard-interrupted unless their backend documents such a wake
path. Harness input EOF should stop accepting new input while allowing active
prompt workers to finish and flush their messages. Explicit disconnect/shutdown
must abort retry sleeps, wake registered backend abort wakers, and detach/finish
without leaving the harness waiting for a provider terminal path.

## Terminal provider failures

Adapters, not display text, classify canonical provider rejection envelopes. The built-in
scheduler bypasses all retry and delay paths for typed terminal failures, including context-window
rejection, even when required-work retries are configured without a practical limit. Raw provider
bodies never become the typed category.

### Watcher-visible provider work

The provider publishes closed retry categories and bounded retry facts. Harness snapshotting, fanout, deduplication, and content-exposure behavior are governed by [SPEC-agent-watch](../../../specs/SPEC-agent-watch.md).
