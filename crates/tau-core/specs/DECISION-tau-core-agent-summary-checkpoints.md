# DECISION-tau-core-agent-summary-checkpoints: Journal-derived agent listing

Authority: confirmed, 2026-07-18, dpc

Agent journals remain the authority for durable identity and listing summaries.
The atomically replaced `meta.json` is only a versioned, journal-bound checkpoint;
Tau does not maintain a second index or global agent catalog.

Journal append and sync precede checkpoint replacement. Listing uses bounded
checkpoint validation and repair rather than unbounded journal scans, but a
missing, stale, corrupt, or over-budget checkpoint must not hide an otherwise
valid journal-backed agent. Strict load and explicit recovery remain fail-closed.

This favors one durable authority and bounded listing cost over independently
queryable indexes. The exact checkpoint, repair, legacy, and summary contracts
are described by [ARCH-tau-core](ARCH-tau-core.md) and
[SPEC-tau-harness-session-state](../../tau-harness/specs/SPEC-tau-harness-session-state.md).
The change is governed by
[DECISION-persistence-and-extension-interface-change-approval](../../../specs/DECISION-persistence-and-extension-interface-change-approval.md).
