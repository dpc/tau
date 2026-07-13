# Built-in provider quota lifecycle

ChatGPT quota credentials remain in this in-process extension and never enter
protocol events, logs, or transcripts. The single runtime loop owns profile
epochs, sequences, bounded merge state, fetch coalescing, and refresh/reset
scheduling. Worker results carry their captured epoch/profile identity and are
discarded after rotation. Every prompt and retry-time credential reload
reconciles the profile before rolling observations can commit. Quota I/O is
best-effort and independent from prompt concurrency and retry budget.

The cross-component authority and no-guessed-applicability rule are defined by
[DESIGN-provider-quota-pacing](../../specs/DESIGN-provider-quota-pacing.md).
