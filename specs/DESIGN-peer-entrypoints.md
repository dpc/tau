# DESIGN-peer-entrypoints: Peer rendezvous uses explicit group authority

Status: confirmed, 2026-07-12, tau-agent-44tt user approval

Cross-session rendezvous is opt-in on at most one effective role group per
harness. `peer_entrypoint: {}` grants discovery and point-to-one bare routing;
the independent nullable `auto_start_role` grant names the enabled member that
may incur model cost when no eligible endpoint exists. Group order never grants
spending authority. Higher-precedence `null` can remove either the whole
entrypoint or only auto-start.

Discovery and caller authority remain independent. `session_list` and
current-session-only `agent_list` are disabled-by-default tools in separate
`session_discovery` and `agent_discovery` groups. Discovery exposes bounded,
redacted, racy harness-authored snapshots. Runtime metadata is only an untrusted
candidate hint; a live target RPC remains authoritative.

Bare routing selects exactly one eligible endpoint and never enumerates remote
agents. Existing exact cross-session addressing remains available to callers
that already know an agent id, so entrypoint opt-in is an accidental
coordination boundary rather than a same-UID security sandbox.

Authentication of the sender's exact pending request must precede endpoint
selection and any auto-start spend. Delivery retries use a durable two-sided
transaction keyed by sender session, sender agent, and logical message id.
Selection, one in-flight auto-start reservation, bounded queued input, role
availability, and active-session generation are target-harness-owned.

Manager roles and task-brokering policy are deliberately outside this decision;
they can compose these ordinary tools and role-group settings later without a
manager-specific protocol concept.

This decision refines
[ARCH-external-message-boundary](ARCH-external-message-boundary.md) and the
component behavior in
[DESIGN-tau-harness-cross-harness-messaging](../crates/tau-harness/specs/DESIGN-tau-harness-cross-harness-messaging.md).
