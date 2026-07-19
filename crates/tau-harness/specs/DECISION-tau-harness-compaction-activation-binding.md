# DECISION-tau-harness-compaction-activation-binding: Durable compaction and activation binding

Authority: confirmed, 2026-07-12, user

Every new inference checkpoint durably binds provider-qualified model, inference
operation, and activation cut as one complete ownership tuple. A standalone
compaction binds its exact transaction, cut, suffix, prompt, model, and operation.
Mutable configuration and runtime connection identity cannot rewrite that
captured authority.

Cuts are provider-valid closed prefixes that keep tool calls and their terminal
results indivisible. Recovery may preserve or retreat a cut on the same ancestor
path but may not advance it, cross branches, or drop owed context. Committed
activation provenance alone determines inference activation; dispatch-uncertain
checkpoints are not resent or silently rerouted.

This binding prevents mutable configuration, crash recovery, and concurrent
suffix facts from changing already-committed provider work. Its cost is strict
fail-closed recovery when correlations or routes are incomplete.

The transaction, replay, recovery, and dispatch contracts are specified by
[SPEC-compaction-and-context-recovery](../../../specs/SPEC-compaction-and-context-recovery.md).
