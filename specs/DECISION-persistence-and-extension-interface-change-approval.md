# DECISION-persistence-and-extension-interface-change-approval: Approval before persistence and extension-interface changes

Authority: confirmed, 2026-07-17, dpc

Every architectural or externally meaningful functional change to an event log
or journal, or to a harness interface with extensions, must first be described
in its own separately reviewed `DECISION-*` record and confirmed by a user or
maintainer. Approval must precede implementation. The change must not be hidden
in or bundled with unrelated feature, bug-fix, refactor, extension, provider, or
UI work.

For event persistence, this includes changes to schemas and semantic facts,
framing, sequencing and ordering authority, write and commit authority,
durability and atomicity, replay and folding, versioning and migration,
concurrency, indexing, retention and compaction, recovery, or coupling among
producers, protocol types, publication, stores, and consumers. The gate applies
to durable semantic and restore journals, and to operational or debug logs when
their externally meaningful behavior changes.

For harness-extension interfaces, this includes changes to protocol and RPC
surfaces, events and payloads, capabilities and authority, startup and
lifecycle semantics, tool naming, registration and routing, trust boundaries,
or persistence and replay contracts. The gate applies whichever component
implements a change to the shared interface.

Pure bug fixes and refactors are exempt only when they preserve documented
semantics. Editorial documentation corrections are exempt when they do not
redefine behavior. If work would alter behavior, choose new behavior for an
undocumented ambiguity, or drift from the documented contract, stop and obtain
the separately approved decision.

This gate keeps persistence recovery guarantees and extension compatibility,
authority, and trust decisions reviewable in isolation. The cost of an explicit
small decision is preferred over hidden cross-component drift.

The confirmed extension-published message-fact interface and persistence
application is recorded by
[DECISION-extension-published-message-facts](DECISION-extension-published-message-facts.md).
The confirmed default for extension-owned state derivable from committed facts is
[DECISION-event-log-first-extension-state](DECISION-event-log-first-extension-state.md).
