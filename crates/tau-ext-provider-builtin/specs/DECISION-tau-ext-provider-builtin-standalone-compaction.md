# DECISION-tau-ext-provider-builtin-standalone-compaction: Explicit standalone compaction

Authority: unconfirmed

Model-callable compaction always uses the existing standalone-compaction
operation and exact provider-qualified model captured by the harness. There is
no inline fallback. A missing route or unsupported model is rejected before
acceptance; provider terminal errors, including context-window rejection, are
one terminal transaction failure and are not retried indefinitely.

This avoids ambiguous inline fallback and unbounded retry after one explicit
transaction has taken ownership. Exact transaction and rejection behavior is
specified by
[SPEC-compaction-and-context-recovery](../../../specs/SPEC-compaction-and-context-recovery.md).
