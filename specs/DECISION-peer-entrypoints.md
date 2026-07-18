# DECISION-peer-entrypoints: Explicit peer-entrypoint and auto-start authority

Authority: confirmed, 2026-07-12, user

Cross-session rendezvous is opt-in on at most one effective role group per
harness. A `peer_entrypoint` grant authorizes discovery and point-to-one bare
routing; the independent nullable `auto_start_role` grant names the enabled member
that may incur model cost when no eligible endpoint exists. Group order never
grants spending authority, and higher-precedence configuration can remove either
grant.

Discovery, exact-address knowledge, bare routing, and auto-start spending remain
separate authorities. Bare routing selects one eligible endpoint and never
enumerates remote agents. Exact cross-session addressing remains available to
callers that already know an agent ID, so entrypoint opt-in is an accidental
coordination boundary rather than a same-UID security sandbox.

Peer delivery is cooperative same-UID, best-effort at-least-once IPC. Callback
correlation precedes endpoint selection, input admission, and any auto-start spend;
selection and live single-flight creation remain target-harness-owned. Tau does
not promise distributed exactly-once delivery or restart deduplication.

Exact discovery, routing, admission, and failure behavior is specified by
[SPEC-tau-harness-peer-discovery](../crates/tau-harness/specs/SPEC-tau-harness-peer-discovery.md)
and [SPEC-tau-harness-peer-routing](../crates/tau-harness/specs/SPEC-tau-harness-peer-routing.md),
within [ARCH-external-message-boundary](ARCH-external-message-boundary.md).
