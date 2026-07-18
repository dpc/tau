# DECISION-tau-core-agent-summary-checkpoints: Journal-derived agent listing

Authority: confirmed, 2026-07-18, dpc

Agent journals are the authority for durable identity and listing summaries.
The existing per-agent `meta.json` is a versioned, atomically replaced
checkpoint over `events.cbor`; Tau does not maintain a second per-agent index or
a global catalog.

A v2 checkpoint binds its summary to the agent id, journal device/inode, exact
post-frame byte offset, next sequence, and a BLAKE3-128 witness over up to 64
bytes at the covered boundary. The summary contains optional microsecond
creation, last-durable-record, and accepted-visible-interaction timestamps plus
the display name. It intentionally does not duplicate prompt text.

Every append commits and syncs the journal first, folds the in-memory summary,
then atomically renames a private same-directory temporary JSON file. A
checkpoint failure never makes an already durable append retry; the loaded
store retains its projection and retries on later writes. Raw message-fact
appends advance the checkpoint exactly like semantic transcript appends.

Fresh listing performs one small JSON read and one journal `stat` per agent and
reads no journal payload. A same-file stale checkpoint may repair under the
agent lock after validating its boundary and contiguous sequence. Foreground
repair is limited to 256 KiB and 64 records per agent. Missing, corrupt,
replaced, truncated, or over-budget state remains a visible journal-backed row
rather than hiding the agent or performing an unbounded scan. Strict agent load
and explicit recovery remain fail-closed and never modify a journal.

`AgentStarted` is the first committed fact for a newly durable agent, before its
route or session membership is exposed. `meta.json`-only artifacts reserve ids
but are not routing identities. Accepted visible UI submissions append the
content-free `AgentUserInteractionRecorded` fact, including queued submissions
that can later be recalled. Live untargeted shell routing uses a harness-local
monotonic acceptance ordinal, not sidecar wall time.

Legacy unversioned JSON is only an unverified display hint. A valid legacy
journal is upgraded during strict load/write or bounded repair. Legacy
metadata-only directories are retained as visible reserved artifacts and are
never synthesized into agents.

## Linked Specs

- [ARCH-tau-core](ARCH-tau-core.md)
- [DECISION-tau-core-semantic-store-durability](DECISION-tau-core-semantic-store-durability.md)
- [ARCH-tau-proto](../../tau-proto/specs/ARCH-tau-proto.md)
- [SPEC-tau-harness-session-state](../../tau-harness/specs/SPEC-tau-harness-session-state.md)
- [SPEC-tau-harness-prompt-dispatch](../../tau-harness/specs/SPEC-tau-harness-prompt-dispatch.md)
- [DECISION-persistence-and-extension-interface-change-approval](../../../specs/DECISION-persistence-and-extension-interface-change-approval.md)
