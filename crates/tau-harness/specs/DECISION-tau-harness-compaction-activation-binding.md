# DECISION-tau-harness-compaction-activation-binding: Durable compaction and activation binding

Authority: confirmed, 2026-07-12, user

Every new inference checkpoint durably binds provider-qualified model, inference
operation, and activation cut as one complete ownership tuple. A standalone
compaction start and successful boundary bind the exact transaction, cut, suffix
end, pre-minted prompt, model, and standalone operation. Its continuation
checkpoint must match the start model and cut and use inference operation.
Legacy all-absent checkpoint ownership tuples remain replay-compatible but
cannot supply current model ownership. Mutable model selection and runtime
connection identity cannot rewrite captured authority.

Every provisional activation or compaction cut is normalized to a
provider-valid closed prefix. A tool-calling assistant node and its complete
terminal-results node remain indivisible, while the owed suffix is retained
through its full resume watermark. Explicit recovery may preserve or retreat a
failed cut along the same ancestor path, but may not advance, cross branches, or
drop the owed watermark.

Committed activation provenance, not prompt text, peers, or interceptors,
determines whether context activates inference. A checkpoint without a durable
terminal response is dispatch-uncertain and is not resent; if an exact captured
route disappears before delivery, the owner is durably terminalized rather than
silently rerouted.

This binding prevents mutable configuration, crash recovery, and concurrent
suffix facts from changing already-committed provider work. Its cost is strict
fail-closed recovery when correlations or routes are incomplete.

The transaction, replay, recovery, and dispatch contracts are specified by
[SPEC-compaction-and-context-recovery](../../../specs/SPEC-compaction-and-context-recovery.md).
