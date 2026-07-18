# ARCH-tau-core: Tau core state and store boundaries

`AgentStore` owns per-agent semantic trees and `SessionStore` owns session
membership/event streams. Each supports durable journals and selected
process-lifetime memory streams. Durable records advance their on-disk sequence;
memory-only membership does not consume that cursor.

A durable session keeps ephemeral-agent loads and matching unloads in a separate
process-local, independently sequenced overlay. Late same-daemon replay first
validates and folds the durable snapshot, then validates and composes the
overlay. Cached membership never bypasses durable journal validation, and restart
discards the overlay with the corresponding ephemeral transcripts.

Store IDs used as path components share one bounded safe grammar with CLI
minting, metadata listing, lock probes, and cleanup. They exclude path separators,
NUL, and the reserved `.` and `..` names.

Durability mode is governed by
[DECISION-tau-core-semantic-store-durability](DECISION-tau-core-semantic-store-durability.md).
