# DESIGN-persistence-and-extension-interface-change-approval: Approval before persistence and extension-interface changes

Status: confirmed, 2026-07-17, dpc

Every architectural or externally meaningful functional change to an event log
or journal, or to a harness interface with extensions, must first be described
in its own standalone `DESIGN-*` record and explicitly approved by a user or
maintainer. The approval must precede implementation. Such a change must not be
bundled into unrelated feature, bug-fix, refactor, extension, provider, or UI
work.

For event persistence, this guard covers schema and semantic-fact selection,
record framing, sequencing and ordering authority, write/commit authority,
durability and atomicity, replay and folding, versioning and migration, locking
and writer concurrency, indexing, retention and compaction, corruption recovery,
and coupling among event producers, protocol types, harness publication, stores,
and consumers. It applies to durable semantic and restore journals as well as
operational or debug event logs when their externally meaningful behavior
changes.

For harness-extension interfaces, this guard covers protocol and RPC surfaces,
events and payloads, capability declaration and authority, startup/readiness,
disconnect/restart and other lifecycle semantics, tool naming/registration and
routing, trust boundaries, and any persistence or replay contract exposed across
the boundary. A change implemented in `tau-proto`, `tau-client`, the harness, an
extension, or another component remains governed when it changes that shared
interface.

Pure bug fixes and refactors are exempt only when they preserve documented
semantics. Editorial documentation corrections are likewise exempt when they do
not redefine behavior. If work would alter behavior, resolve an undocumented
ambiguity by choosing new behavior, or drift from the documented contract, stop
and obtain the standalone approved design record rather than treating the change
as an incidental fix.

This gate keeps persistence recovery guarantees and extension compatibility,
authority, and trust decisions reviewable in isolation. The cost of an explicit
small design change is preferred over hidden cross-component drift.

The confirmed extension-published message-fact interface and persistence
application is recorded by
[DESIGN-extension-published-message-facts](DESIGN-extension-published-message-facts.md).
