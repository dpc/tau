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

Codex OAuth response parsing, byte caps, and credential-safe error formatting
are governed by [`tau-provider-codex/SECURITY.md`](../tau-provider-codex/SECURITY.md). This
extension logs only the typed error's default safe projection and never its
untrusted parsed provider fields.

Permanent refresh suppression is memory-only and contains secret credential
copies solely for exact equality; its types have no credential-revealing debug
projection. The auth-file lock serializes reload and refresh, and the generation
loaded under that lock replaces any stale caller snapshot before valid-only
fallback. Closed credential-invalidating 400/401 codes may suppress the exact
generation for this process. Profile rotation clears it; restart may retry once.

The cross-component authority and no-guessed-applicability rule are defined by
[DECISION-provider-quota-pacing](../../specs/DECISION-provider-quota-pacing.md).

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

The worker and transport cancellation choice is recorded by
[DECISION-tau-ext-provider-builtin-bounded-prompt-workers](specs/DECISION-tau-ext-provider-builtin-bounded-prompt-workers.md).
