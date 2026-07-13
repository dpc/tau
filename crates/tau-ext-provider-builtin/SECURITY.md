# tau-ext-provider-builtin security boundaries

## Quota lifecycle

ChatGPT quota credentials remain in this in-process extension and never enter
protocol events, logs, or transcripts. The single runtime loop owns profile
epochs, sequences, bounded merge state, fetch coalescing, and refresh/reset
scheduling. Worker results carry their captured epoch/profile identity and are
discarded after rotation. Every prompt and retry-time credential reload
reconciles the profile before rolling observations can commit. Quota I/O is
best-effort and independent from prompt concurrency and retry budget.

The cross-component authority and no-guessed-applicability rule are defined by
[DESIGN-provider-quota-pacing](../../specs/DESIGN-provider-quota-pacing.md).

## Provider cancellation boundary

Broadcast cancellation uses two synchronized generations. The provider runtime
generation rejects or replaces late worker output and retry outcomes, while the
shared cancellation generation aborts active backend and retry checks. Abort
wakers are snapshotted under the cancellation mutex and invoked after unlocking.
Registration compares its captured generation under the same mutex, ensuring
that it is either included in a later broadcast snapshot or immediately observes
an already-completed broadcast without a lost wakeup. Cancellation remains
cooperative for transports that do not register an abort waker.

The worker and transport cancellation design is recorded by
[DESIGN-tau-ext-provider-builtin-bounded-prompt-workers](specs/DESIGN-tau-ext-provider-builtin-bounded-prompt-workers.md).
