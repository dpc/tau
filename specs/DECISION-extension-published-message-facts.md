# DECISION-extension-published-message-facts: Extension-published message facts

Authority: confirmed, 2026-07-17, dpc

Immutable external-message facts use ordinary extension `Emit` and the normal
persist-before-consume event path. Prompt projection, UI, bridge publishers, and
other extensions are peer consumers of the same committed facts; no consumer
owns a replacement publication.

Tau deliberately has no generic transport admission, canonical mutable message
object, route registry, message authorization, completion RPC, native-message
reconciliation, or exactly-once service. Transport relevance, identity,
deduplication, routing, reply/send authority, retries, and actionable
capabilities remain extension-local. The harness contributes only authenticated
publisher provenance and ordinary event persistence, validation, replay, and
projection.

This keeps the shared schema transport-neutral and makes committed history
independent of a bridge's runtime while avoiding a second messaging subsystem.
The tradeoff is that facts may be duplicated, unresolved, or mutually
inconsistent, and remote send, event commit, and tool completion are not one
transaction.

The exact schema, bounds, persistence, replay, and projection behavior is
specified by
[SPEC-extension-published-message-facts](SPEC-extension-published-message-facts.md).
This choice is governed by
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md).
