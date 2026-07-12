# DESIGN-tau-ext-provider-builtin-standalone-compaction: Explicit standalone compaction

Status: unconfirmed

Model-callable compaction always uses the existing standalone-compaction
operation and exact provider-qualified model captured by the harness. There is
no inline fallback. A missing route or unsupported model is rejected before
acceptance; provider terminal errors, including context-window rejection, are
one terminal transaction failure and are not retried indefinitely.
