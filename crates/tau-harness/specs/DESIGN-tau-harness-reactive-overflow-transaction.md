# DESIGN-tau-harness-reactive-overflow-transaction: Reactive overflow transaction

Status: confirmed, 2026-07-11, dpc

Eligible ordinary inference overflow is durably recorded before one correlated standalone compaction starts. The compact transaction claims the failed prompt, compacts only through the pre-activation cut, and resumes through the original checkpoint so concurrent suffix facts and the owed activation are preserved. Any second overflow or ambiguous dispatch is terminal rather than recursive.

Testing is split by owner: `tau-proto` fixes default and tagged wire forms; `tau-core` fixes unique claim validation and planned/claimed replay folding; `tau-harness` covers eligibility, durable ordering, no-recursion, continuation, watcher projection, and crash cuts.
