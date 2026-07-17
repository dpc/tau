# DESIGN-tau-core-semantic-store-durability: Semantic stores can be durable or memory-only

Status: unconfirmed

`AgentStore` and `SessionStore` both support normal durable event streams and
selected memory-only streams used by ephemeral agents/sessions. The memory-only
path must fold the same semantic facts for live replay while avoiding creation of
reserved state directories, sidecars, locks, and event files.

Testing strategy: every new semantic write path should cover both persistence
modes, including a negative filesystem assertion for memory-only records and a
positive replay/folding assertion that the in-memory state still behaves like the
durable equivalent while the process lives.

Durable `events.cbor` replay is fail-closed. Stores verify record framing,
monotonic durable sequence numbers, path-safe store ids, and the same semantic
event/parent invariants enforced for live appends before folding replayed state.
Corrupt, truncated, spliced, or semantically invalid durable records must return a
typed store error instead of being ignored or panicking during fold.

Only one real background completion is valid for a globally unique tool call id:
once either `ToolBackgroundResult` or `ToolBackgroundError` has been recorded,
later background completion events for that id are rejected during both live
append and durable replay. Duplicate detection is global by `ToolCallId`; the
known-call check remains branch-relative and must resolve the event's explicit
fold parent instead of using the mutable global tree head.

Durable sequence numbers count only records actually written to the corresponding
`events.cbor` stream. Memory-only session membership facts update live folded
state but do not advance the durable sequence cursor, so later durable records
remain contiguous on disk.

Durable session ids are store path components. The path-component grammar is
shared by CLI session-id minting, store validation, metadata listing, lock
probes, and cleanup: minted ids must be bounded and must not contain path
separators, NUL, or the reserved `.`/`..` names.
