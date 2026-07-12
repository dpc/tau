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

Callback correlation of the sender's exact pending request must precede bare
endpoint selection and any auto-start spend. Peer delivery is cooperative
same-UID, best-effort at-least-once IPC: live success waits for the exact receive
projection to commit, but crash ambiguity may duplicate delivery, agent/model
work, or spend. No distributed WAL, restart resumption, or cross-session
exactly-once deduplication is required. Selection, live single-flight auto-start,
bounded queued input, role availability, and active-session generation are
target-harness-owned.

Current implementation state: `auto_start_role` is validated configuration
only. Bare routing uses an already loaded or pending eligible endpoint,
always reports `started: false`, and fails privately when none exists. No peer
message can start an agent until the separately reviewed auto-start phase lands.

Manager roles and task-brokering policy are deliberately outside this decision;
they can compose these ordinary tools and role-group settings later without a
manager-specific protocol concept.

This decision refines
[ARCH-external-message-boundary](ARCH-external-message-boundary.md) and the
component behavior in
[DESIGN-tau-harness-cross-harness-messaging](../crates/tau-harness/specs/DESIGN-tau-harness-cross-harness-messaging.md).
