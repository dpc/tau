# DECISION-persistence-and-extension-interface-change-approval: Approval before persistence and extension-interface changes

Authority: confirmed, 2026-07-17, dpc

## Decision

Every architectural or externally meaningful change to an event log, journal,
or harness-extension interface must first be described in its own separately
reviewed `DECISION-*` record and confirmed by a user or maintainer. Approval must
precede implementation and must not be hidden in unrelated work.

This includes persistence schema, ordering, durability, replay, recovery, and
indexing decisions, plus shared protocol, capability, lifecycle, tool naming,
routing, authority, and trust-boundary changes.

Pure bug fixes, refactors, and editorial corrections are exempt only when they
preserve documented semantics. If work would choose behavior for an ambiguity or
change the contract, it must stop for approval. The explicit review cost is
preferred over hidden cross-component drift.
