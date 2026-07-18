# DECISION-event-log-first-extension-state: Event-log-first durable extension state

Authority: confirmed, 2026-07-18, dpc

When extension-owned state must survive restart and committed Tau facts can
completely represent its non-secret inputs, the ordered Tau journal is the sole
durable source of truth. Extensions derive bounded indexes, reverse lookups,
dependency maps, and tombstone state by folding committed facts in journal delivery
order, then apply the same fold to live delivery. They do not maintain a parallel
durable snapshot, sidecar, promotion ledger, or unbounded shadow “seen” set.

This default keeps ordering, commit, durability, replay, and corruption recovery
under one authority. It avoids crash consistency and reconciliation protocols
between two stores at the cost of replay work and explicit retention, capacity,
conflict, and readiness semantics in each adopting feature.

An adopter may encode information omitted from generic fact fields in one bounded,
versioned, publisher-owned `extension_data` schema. Generic infrastructure enforces
only global structural limits and remains schema-opaque. The publisher defines
stricter bounds and decoding, and unknown or malformed versions fail closed for the
derived feature without invalidating the generic committed fact. Such data must be
safe for trusted raw journal readers and every matching trusted subscriber; it must
not contain credentials, bearer values, reusable capabilities, or other secrets.
Current configuration and authorization are revalidated at use time rather than
serialized as authority.

Each adopting specification defines the source facts and scope, exact fold and
conflict rules, bounded capacity and deterministic eviction, deletion and
non-resurrection behavior, replay/live handoff, readiness gate, and the effects of
journal retention, compaction, or replacement. It also distinguishes reconstructible
indexes from process-only reservations, in-flight work, and authority not proven by
a fact. Combining independent journals requires a defined stable total order; a
process-global order must not be inferred from per-journal sequences or timestamps.

Only a committed fact is durable fold input. State created before publication or
after local extension output but before harness append is transient. Append before
live self-delivery is safe because replay reconstructs the state. Interrupted replay
discards its partial fold, and route-dependent operation remains unavailable until
the required error-free replay boundary has completed. A bounded provisional live
entry may close a publisher's self-delivery race, but it is never recovery authority.
Replay itself performs no remote effects.

Event-log-first state does not make a remote effect transactional with fact commit or
tool completion, and it does not imply exactly-once delivery, reconciliation, or an
outbox. Separately justified private persistence remains appropriate for secrets and
credentials, large blobs, data unsafe for matching trusted subscribers, remote-effect
intent requiring restart guarantees, state whose invariants outlive available
history, or measured replay/latency needs. Any duplicate durable state requires an
explicit rationale, synchronization and crash contract, and a separately confirmed
decision under
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md).

A Slack `message_ref` route map is one illustrative adopter: inert, bounded native
route metadata could be published in Slack-owned `extension_data` and folded into a
lookup while every use rechecks current Slack policy. This decision does not itself
change Slack behavior or move route schemas, policy, capabilities, or native
interpretation into the harness.

The owning component boundaries are described by
[ARCH-tau](ARCH-tau.md),
[ARCH-tau-core](../crates/tau-core/specs/ARCH-tau-core.md),
[ARCH-tau-harness](../crates/tau-harness/specs/ARCH-tau-harness.md), and
[ARCH-external-message-boundary](ARCH-external-message-boundary.md).
