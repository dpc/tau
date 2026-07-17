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

The transport-ingress derived-index rebuild may structurally validate and skip a
uniformly legacy agent journal whose records carry the historical `id` marker.
Those journals predate typed transport ingress, so unrelated payloads need not
remain decodable under the current event schema. Sequenced journals likewise
decode only the stable event-name discriminator and typed incoming records while
validating every record's explicit sequence. This does not make an otherwise old
journal resumable through ordinary agent replay. A mixed or unmarked encoding,
an incorrect explicit sequence, malformed event-name structure, or typed incoming
decode failure remains a typed fail-closed error.

Raw-CBOR tests in `agent_store/tests.rs` own this structural compatibility
boundary: marker presence/type/uniqueness, explicit sequence validation, event
discriminator grammar, selective incoming decode, and both mixed-encoding
orders. Multi-journal locator rebuild and global dedup ownership belong to the
harness lifecycle tests. See
[`DESIGN-canonical-transport-ingress`](../../../specs/DESIGN-canonical-transport-ingress.md)
for the system authority contract.

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
