# ARCH-tau-ext-provider-builtin: tau-ext-provider-builtin architecture

`tau-ext-provider-builtin` is Tau's built-in provider bridge. It resolves an
immutable credential-free settings snapshot and runtime Secret RPC credentials, publishes available models, receives
model-visible prompt and tool context from the harness, invokes external model
services, and reports provider execution through Tau protocol events.

## Ownership boundaries

Tau loads credential-free profiles as a disjoint union from XDG config and state
at `providers/<extension>/<namespace>.json`; each profile's serialized kind
selects its backend family. Typed credentials live separately in the selected
extension instance's Secret scope. ChatGPT profiles use the
model matrix and finite inference facade owned by `tau-provider-codex`; Chat
Completions and OpenRouter profiles use the Chat Completions backend. Generic
public `responses` profiles use the separate API-key Responses backend with
per-profile HTTP/SSE or WebSocket transport and always fully replay their typed
transcript. This public WebSocket route remains separate from private Codex and
OpenAI Realtime. The
extension owns immutable startup-profile resolution, model publication, event ordering,
public response sampling, logical retries, cancellation, and supervised prewarm
work. Backends return typed outcomes and never serialize harness frames.

Chat Completions routes select cache-usage parsing only through their serialized
compatibility capability. A selected cache schema requires streamed usage, so the
adapter sends its existing `stream_options.include_usage` request member rather
than relying on an undocumented terminal chunk. OpenRouter's built-in and
discovered route capabilities select its documented OpenAI-compatible
read/write counters and streamed usage, but publish no cache contract or
cache-control request behavior. OpenRouter can select a different upstream
provider for a request, so its counters cannot establish an upstream cache
mechanism, residency, privacy posture, renewal operation, or lifecycle.

ChatGPT profiles capture Responses mode at process startup. Model publication,
prompt, prewarm, retry, and quota resolution share that value; credential reload
and OAuth refresh do not change it. An on-disk mode edit takes effect after
restart, and different namespaces may select different modes. The selected
Codex surface is constrained by
[GATE-tau-provider-codex-responses-surface-selection](../../tau-provider-codex/specs/GATE-tau-provider-codex-responses-surface-selection.md).

The main runtime loop owns ChatGPT quota profile epochs and reconciliation.
Prompt workers only report normalized observations through the worker channel;
quota failures neither delay inference nor consume prompt retry budget. This
follows
[GATE-provider-quota-pacing](../../../specs/GATE-provider-quota-pacing.md).

## Provider and credential boundary

External provider responses are untrusted prompt-surface data. Streamed text,
reasoning, tool arguments, and custom-tool input must not be copied into
notices, traces, or final transcript rendering. Codex retry status may carry
only its opaque, single-line, bounded `RedactedProviderDetail`; the extension
cannot construct one from raw error display. Public response stats are
content-free, prompt-local transport metadata; the extension owns their sampling
and the harness validates and broadcasts them under
[SPEC-provider-response-streaming](../../../specs/SPEC-provider-response-streaming.md).
Provider execution output uses transient `_reported` events, from which the
harness derives canonical facts under
[SPEC-provider-execution-reports-and-canonical-facts](../../../specs/SPEC-provider-execution-reports-and-canonical-facts.md).

The provider boundary cannot authorize transcript mutation or compaction.
Built-in providers may classify a context-window failure, but the harness clears
provider-supplied recovery claims and independently derives recovery eligibility
from its prompt, model, operation, policy, and branch state.

Each `ChatGptPromptExecutionContext` owns the one-based logical attempt
ordinal. A new APID begins at one; a manual retry preserves the prompt-local
failure count. Transparent WebSocket repair increments only the per-attempt
wire-dispatch index. The extension does not add provider detail to
`ProviderRetryStatus`, watcher state, agent messages, durable terminal facts, or
restore journals.

Profile files, OAuth tokens, and API keys are local secrets. They must not enter
model-visible output, notices, traces, debug logs, or fixtures. Debug captures
may contain full prompt and tool-result content and therefore remain gated by
explicit durable-session policy. New captures are zstd-compressed on one
bounded best-effort background writer; overload, write failure, or process
shutdown can omit captures but never delay provider or UI work. Detailed
credential and response controls are owned by [`SECURITY.md`](../SECURITY.md).

Provider settings never contain OAuth or API-key values. Authenticated profiles
reference typed version-zero credentials in their configured-instance Secret
scope. Supported local-compatible profiles may instead explicitly select
`credential: {"kind":"none"}`; they perform no Secret lookup. Missing
credential selection remains invalid, so credential loss cannot silently become
an unauthenticated request. The extension uses compare-and-swap for rotating
OAuth refresh tokens. Credential rotation is visible without restart; settings
changes require restart. See
[SPEC-extension-secret-storage](../../../specs/SPEC-extension-secret-storage.md).

## Runtime and worker flow

Protocol startup publishes provider kind and subscriptions, declares models,
and then signals readiness. The harness derives canonical model state after
activation. The normal `tau-client` writer is the only protocol serialization
path.

Prompt and prewarm workers enqueue typed messages before waking the manual main
loop with `ManualRuntimeWaker`. Wakes are coalesced, so the loop drains both
harness input and worker messages before blocking again. Prewarm work is
supervised separately from prompt concurrency. Delayed retries share one
scheduler thread and do not retain worker permits; their ownership and cooldown
contract is specified by
[SPEC-tau-ext-provider-builtin-retry-scheduler](SPEC-tau-ext-provider-builtin-retry-scheduler.md).

Prompt cancellation is cooperative. Queued work can be removed immediately,
retry sleeps can be aborted, and backends may register prompt-specific transport
wakers. Input EOF stops new work while allowing active workers to flush; explicit
disconnect or shutdown cancels pending work and wakes registered transports.
After terminal disconnect, detached workers retain finite network bounds but no
longer reconcile into the dropped runtime loop.

Harness-scheduled cache refreshes remain subordinate to real prompts and the
Provider's shared cooldown. The Provider correlates each bounded request and
cancellation, enforces a receipt-relative fail-safe deadline, and emits exactly
one content-free terminal report. A cooled Provider returns failure without
changing or releasing its retry cohort. Existing supervisor exact-key
deduplication, global capacity, profile rotation, cancellation, and shutdown
bounds remain defense in depth below the harness scheduler, which alone owns
one-per-Provider admission. See
[SPEC-provider-cache-refresh-lifecycle](../../../specs/SPEC-provider-cache-refresh-lifecycle.md).

Adapters classify typed terminal provider failures independently of display
text. Terminal failures bypass retries, and raw provider bodies never become
closed failure categories. Watcher projection of bounded provider work is
governed by [SPEC-agent-watch](../../../specs/SPEC-agent-watch.md).

Public Responses WebSocket terminal code, type, or incomplete-reason detail is
bounded to 128 Unicode scalars and may enter the final failure diagnostic after
visible escaping of controls and terminal-unsafe Unicode. Ordinary printable
detail remains intact: Tau deliberately applies no pattern-based secret
scrubbing. An operator-configured provider can therefore theoretically reflect
sensitive content in that bounded diagnostic. The visible display projection
does not affect the closed failure category, recovery, or retry decision:
classifiers retain the original bounded detail.
