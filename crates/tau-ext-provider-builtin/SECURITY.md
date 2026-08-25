# tau-ext-provider-builtin security boundaries

## Quota lifecycle

ChatGPT quota credentials remain in this in-process extension and never enter
protocol events, logs, or transcripts. The single runtime loop owns profile
epochs, sequences, bounded merge state, fetch coalescing, and refresh/reset
scheduling. Worker results carry their captured epoch/profile identity and are
discarded after rotation. Every prompt and retry-time credential reload
reconciles the profile before rolling observations can commit. Quota I/O is
best-effort and independent from prompt concurrency and retry budget.

Shared inference cooldown recovery remains main-loop authorized. Only a
cancellation-validated successful terminal from the exact finite attempt that
probed the current generation may clear it; quota telemetry is never recovery
authority. Scheduler release preserves independent prompt deadlines, delayed
ownership/counting, and unrelated providers or generations. Configured
credential, endpoint, or backend-family identity rotation invalidates only the
old provider profile cooldown.

Parsed usage-window reset hints remain informational and are not scheduler lower
bounds. The runtime probes through the existing per-prompt jittered Fibonacci
cadence (10-second production base and 30-minute generated ceiling) and
same-profile cooldown because access can recover before a reported reset. Shared
cooldowns, up to five seconds of stable prompt-local boundary jitter, and bounded
prompt-worker concurrency limit synchronized load; they do not guarantee one
probe per profile interval when multiple prompt-local retry states are parked.
Revisit this policy if provider load guidance, the concurrency override, jitter,
or shared-cooldown semantics change.

The extension owns serialized Chat Completions/OpenRouter profiles, model
publication, public response sampling, harness event writes, and logical retry.
Both wire backends receive only finite-attempt inputs and return typed outcomes;
they cannot write protocol frames. Codex resolved credentials/configuration are
opaque non-`Debug` values, and backend dispatch/byte/semantic-progress facts
contain no bearer, account, prompt, or provider-body data.

Public Responses attempts bound request/connect/header work and then separately
bound each SSE/WebSocket stream's semantic-idle and absolute lifetimes.
Qualifying semantic output renews only the former; transport keepalives and
control traffic cannot prolong a stalled attempt. Cancellation remains
cooperative during these waits. Revisit this boundary when changing public
Responses timeout, framing, semantic-progress, or cancellation behavior.

Codex OAuth response parsing, byte caps, and credential-safe error formatting
are governed by [`tau-provider-codex/SECURITY.md`](../tau-provider-codex/SECURITY.md). This
extension logs only the typed error's default safe projection and never its
untrusted parsed provider fields.

Permanent refresh suppression is memory-only and keyed by the credential record
generation. Its types have no credential-revealing debug projection. Secret-scope
compare-and-swap serializes refresh publication, and a losing refresher reloads
the winning generation rather than retrying a rotated refresh token. Closed
credential-invalidating 400/401 codes may suppress the exact generation for this
process. Rotation clears it; restart may retry once.
Separately, one canonical inference HTTP 401 authorizes at most one forced
reload/refresh for the exact resolved credential generation, even while its
local expiry remains current. Failure or a refresh that retains the rejected
access token blocks automatic replay for that generation. Refresh publication
and losing-CAS adoption require the same non-empty ChatGPT account identity;
missing, internally inconsistent, or changed identity fails closed before the
prompt can continue. The current credential contract does not retain an ID
token, so user-id and workspace-class claims cannot be pinned reliably when the
refresh response omits that optional token.
The bounded rejected-generation history survives temporary profile removal or
backend-family replacement within the process, preventing remove/re-add from
re-authorizing the same bearer.

The cross-component authority and no-guessed-applicability rule are defined by
[GATE-provider-quota-pacing](../../specs/GATE-provider-quota-pacing.md).

## Runtime cache contract

Generic model `cache_contract` values are operator assertions for one exact
configured route. The extension does not infer TTL, renewal, quota, privacy, or
deletion semantics from endpoint, model/provider name, request controls, cache
usage, OpenRouter routing, or recent hits. Provider-independent contradictions
fail profile parsing, and current production profiles cannot claim a typed
manual-delete operation because no existing backend owns one.

Published contracts contain only closed capability/privacy categories,
durations, quota classes, and adapter-owned prefix version `1`. They contain no
prompt content, cache key, cache/object identifier, region, timestamp, hit
history, or lifecycle state. They use the existing transient provider-model
declaration and current-state path and add no refresh/delete traffic, semantic
journal data, cold replay, or restart reconstruction. Unknown ZDR, residency,
quota, output, price, or TTL facts remain unknown and cannot authorize automated
renewal. A recent hit never becomes hard-TTL evidence.

Generic Chat Completions `extra_body` can carry an opaque reference to a named
object managed outside Tau. Tau preserves the configured request member and
clones it into attempts, but it does not validate its privacy or residency,
model it as cache-object state, or perform create, recovery, PATCH, delete, or
external-storage accounting. The operator owns retention, deletion, residency,
zero-data-retention suitability, and billing for such an object. Native parsing,
lifecycle, or accounting would cross this boundary and requires review before
implementation.

## Scoped credential records

Authenticated settings contain one opaque `credential.identity` and a closed
credential-slot kind. The identity can resolve only to that slot under the
selected extension's Secret scope; it survives a provider-profile filename
rename without making settings point at arbitrary filesystem paths.
Supported local-compatible profiles may instead use the exact explicit
`credential: {"kind":"none"}` marker, which neither creates nor reads a Secret
record. Missing or malformed credential selection remains invalid. For stored
credentials, the runtime reads a typed version-zero OAuth or API-key record
through Secret RPC before publishing that provider's models and again at every
prompt boundary. Unavailable, malformed, wrong-kind, or wrong-version records
exclude the provider. Diagnostics may identify provider names and safe error
categories but never secret paths, values, or host filesystem paths.

Initial Configure validates every filename and full provider profile before
retaining one parsed immutable snapshot or publishing models. One invalid entry
rejects the complete snapshot through exactly one bounded `ConfigError`; the
extension publishes neither models nor `Ready`. Invalid-filename diagnostics
expose no raw filename. Profile-validation diagnostics may identify only the
already-validated provider name and a closed reason. Neither form exposes paths,
raw settings, or values. Prompt-time loading clones the already-validated
snapshot before credential hydration, so it cannot reparse or log malformed
persisted settings. Preserve that invariant if runtime reconfiguration or
snapshot mutation is introduced.

Named API-key sources are setup/login/startup authorities, not runtime credential
inputs. Setup and login resolve the exact configured declaration only while
holding the providers instance lock, then take the Secret lock and publish the
bounded typed record. Setup activates settings last; login instead revalidates
the existing profile's exact source and bytes and changes only its Secret. Login
never writes a profile, follows and preserves supported config leaf symlinks, and
cannot create a shadow state profile. Resolution, validation, or size failure
before atomic replacement leaves the previous credential unchanged.
A post-replacement permission, directory-sync, or lock-release failure may report
failure after the new credential became visible. Harness startup uses the same lock order
and one shared closed reference parser, publishes empty typed records for
unavailable bindings, and retains that locked settings generation for
Configure. Bound declaration values never enter Configure, logs, notices, or
provider settings; warnings expose only configured instance, provider, and
source names.

Changes to login profile identity checks, lock ordering, named-source resolution,
Secret size enforcement, config-symlink handling, or settings mutation require
focused credential-persistence and symlink review.

## Provider cancellation boundary

Broadcast cancellation uses two synchronized generations. The provider runtime
generation rejects or replaces late worker output and retry outcomes, while the
shared cancellation generation aborts active backend and retry checks. Abort
wakers are snapshotted under the cancellation mutex and invoked after unlocking.
Registration compares its captured generation under the same mutex, ensuring
that it is either included in a later broadcast snapshot or immediately observes
an already-completed broadcast without a lost wakeup. Cancellation remains
cooperative for transports that do not register an abort waker.

Fresh ChatGPT WebSocket upgrades use this same linearized abort registry and a
separate bounded provider connection deadline. Their transient connecting event
contains only fixed status text: endpoints, account identifiers, credentials, and
raw transport diagnostics remain inside the provider process.

ChatGPT prewarm uses a separate main-loop-owned supervisor. Duplicate work is
suppressed per provider/target-agent cache owner and total concurrent work is
capped at the default WebSocket pool capacity; real prompts, cancel,
shutdown/disconnect, and profile rotation wake and cancel the worker. The
transport bounds upgrade and response time, and pool invalidation prevents
stale reserved sockets from returning after profile/session boundaries.
Terminal harness disconnect drops loop-side completion tracking only after
cancellation; detached work retains its abort source and finite bounds.
Prewarm cancellation callbacks run under their private registry lock and may
only enqueue a transport wake or invalidate a pool generation. Guard
unregistration joins an already-started callback and is the socket-publication
boundary; callback code must never re-enter that registry.
Provider execution output crosses the trusted local extension boundary as explicit
transient `_reported` events. The harness, not this extension, publishes correlated
canonical provider facts and directed retry outcomes. Terminal report image bytes are
cleared from generic live/debug projections; canonical provider transcript handling
retains its existing policy. See
[SPEC-provider-execution-reports-and-canonical-facts](../../specs/SPEC-provider-execution-reports-and-canonical-facts.md).

## Tau-owned summary compaction fallback

Chat Completions, OpenRouter, and public Responses models advertise standalone
summary compaction by default with conservative context-derived limits and a
proactive threshold. `local_summary_compaction` remains an optional full
per-model override. Tau sends the ordinary provider request prefix for the
immutable cut, including the normal system prompt, tools, history, images, raw
tool arguments, and cache controls, then appends one harness-authored
`<tau_internal>` user message. Any tool call fails without execution. Tau accepts
one nonempty bounded assistant final text, discards reasoning and opaque replay
data, and stores the exact text once as one synthetic user checkpoint without a
wrapper or deterministic supplement. Ordinary opted-in debug capture applies.
Unsupported output, insufficient context, cancellation, route loss, stale state,
and post-output failures end the durable transaction without inference fallback
or ambiguous resend.

Provider settings contain no credentials. The extension reads typed version-zero
OAuth and API-key records only through its configured-instance Secret RPC,
reloads them at prompt time, and persists OAuth rotation with generation
compare-and-swap. Secret frames and decoded records must never enter logs,
events, transcripts, errors, or debug output. This boundary is specified by
[`SPEC-extension-secret-storage`](../../specs/SPEC-extension-secret-storage.md).
Lifecycle-aware cache refresh requests arrive only through trusted configured
extension IPC and can contain the complete previously sent prompt prefix. The
built-in Provider does not log or rebroadcast that payload. It correlates
cancellation by validated refresh id, applies a receipt-relative fail-safe
deadline, and reports only a closed content-free status. Separately enabled
private Provider request capture remains an explicit exception and may contain
the exact upstream refresh request.
