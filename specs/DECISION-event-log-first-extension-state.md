# DECISION-event-log-first-extension-state: Event-log-first durable extension state

Authority: confirmed, 2026-07-18, dpc

When extension-owned state must survive restart and committed Tau facts can
completely represent its non-secret inputs, the ordered Tau journal is the sole
durable source of truth. Extensions derive bounded indexes and tombstone state
by folding committed facts, then apply the same fold to live delivery. They do
not maintain a parallel durable snapshot or unbounded shadow set.

This keeps ordering, commit, replay, and recovery under one authority. It avoids
cross-store reconciliation at the cost of replay work and explicit retention,
capacity, conflict, and readiness semantics.

Only committed facts are durable fold input, replay performs no remote effects,
and current configuration and authorization are revalidated at use time. Bounded
publisher-owned extension data may carry non-secret fold inputs that generic
infrastructure treats as opaque.

This choice does not make remote effects transactional or imply exactly-once
delivery, reconciliation, or an outbox. Private persistence remains appropriate
for secrets, large or subscriber-unsafe data, restart-guaranteed remote intent,
state that outlives retained history, or demonstrated replay constraints.
Duplicate durable state requires separate confirmation under
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md).
