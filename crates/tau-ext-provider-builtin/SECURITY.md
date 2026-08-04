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

The cross-component authority and no-guessed-applicability rule are defined by
[GATE-provider-quota-pacing](../../specs/GATE-provider-quota-pacing.md).

## Scoped credential records

Credential-free settings contain one deterministic `credential.secret_path`.
The runtime reads a typed version-zero OAuth or API-key record through Secret
RPC before publishing that provider's models and again at every prompt boundary.
Unavailable, malformed, wrong-kind, or wrong-version records exclude the
provider. Diagnostics may identify provider names and safe error categories but
never secret paths, values, or host filesystem paths.

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

## Local Chat Completions summary compaction

Standalone summary compaction is available only through an exact model's
explicit `local_summary_compaction` declaration with a matching context window
and bounded input/output limits. The provider runtime never persists the full
compactor request, including debug capture, and never retries after semantic
output. Only validated bounded text can become an explicitly untrusted
user-role historical checkpoint. Re-review locality, budgeting, capture, and
terminalization whenever this profile or dispatch path changes.
Known-remote OpenRouter conversion strips the local-only declaration.
Provider settings contain no credentials. The extension reads typed version-zero
OAuth and API-key records only through its configured-instance Secret RPC,
reloads them at prompt time, and persists OAuth rotation with generation
compare-and-swap. Secret frames and decoded records must never enter logs,
events, transcripts, errors, or debug output. This boundary is specified by
[`SPEC-extension-secret-storage`](../../specs/SPEC-extension-secret-storage.md).
