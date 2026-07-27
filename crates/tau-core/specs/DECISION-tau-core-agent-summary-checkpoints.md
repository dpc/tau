# DECISION-tau-core-agent-summary-checkpoints: Journal-derived agent listing

Authority: confirmed, 2026-07-18, dpc

Agent journals remain the authority for durable identity and listing summaries.
The atomically replaced `meta.json` is only a versioned, journal-bound checkpoint;
Tau does not maintain a second index or global agent catalog.

Complete journal-frame write precedes checkpoint replacement; journal writeback
is asynchronous and the checkpoint is derived, not durability evidence. Listing
uses bounded checkpoint validation and repair rather than unbounded journal scans, but a
missing, stale, corrupt, or over-budget checkpoint must not hide an otherwise
valid journal-backed agent. Locked recovery invalidates a checkpoint when it
truncates an invalid journal suffix.

This favors one durable authority and bounded listing cost over independently
queryable indexes. The exact checkpoint, repair, legacy, and summary contracts
are described by [ARCH-tau-core](ARCH-tau-core.md) and
[SPEC-tau-harness-session-state](../../tau-harness/specs/SPEC-tau-harness-session-state.md).
The change is governed by
[GATE-persistence-and-extension-interface-change-approval](../../../specs/GATE-persistence-and-extension-interface-change-approval.md).
The asynchronous writeback rule supersedes the earlier append-and-sync ordering
under
[SPEC-semantic-journal-writeback-durability](../../../specs/SPEC-semantic-journal-writeback-durability.md).
