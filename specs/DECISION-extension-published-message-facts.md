# DECISION-extension-published-message-facts: Extension-published message facts

Authority: confirmed, 2026-07-17, dpc

## Status

The current implementation still follows this record's direct canonical
`message.*` publication, publisher stamping, interception bypass, and forced
durability rules. [DECISION-generic-peer-event-emission](DECISION-generic-peer-event-emission.md)
replaces those rules with generic transient bridge reports followed by
harness-authored canonical message facts. Until that migration completes, the
direct-publication clauses here remain the current contract.

The transport-neutral schema and extension-local ownership of transport
authentication, admission, deduplication, native routing, replies, retries, and
capabilities remain authoritative during and after the migration.

Immutable external-message facts use ordinary extension `Emit` and the normal
persist-before-consume event path. Prompt projection, UI, bridge publishers, and
other extensions are peer consumers of the same committed facts; no consumer
owns a replacement publication.

Tau deliberately has no generic transport admission, canonical mutable message
object, route registry, authorization, reconciliation, or exactly-once service.
Transport identity, deduplication, routing, reply/send authority, retries, and
capabilities remain extension-local. The harness contributes authenticated
publisher provenance plus ordinary validation, persistence, replay, and projection.

This keeps the shared schema transport-neutral and avoids a second messaging
subsystem. The tradeoff is that facts may be duplicated, unresolved, or mutually
inconsistent, and remote send, event commit, and tool completion are not one
transaction. Exact behavior is specified by
[SPEC-extension-published-message-facts](SPEC-extension-published-message-facts.md).
