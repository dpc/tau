# DECISION-current-session-control-rpc: Query current session identity from live harness memory

Authority: confirmed, 2026-07-21, dpc

## Decision

`tau session list` must use a narrow requester-directed
`get_current_session`/`current_session_result` control RPC to obtain each
responsive local harness's current session ID and canonical startup project
root. Live harness memory is authoritative; socket paths locate candidates
only, and adjacent metadata or persisted journals do not supply listing rows.

## Rationale

Only a live harness can authoritatively report its current session identity;
filesystem artifacts can be stale or describe historical state. Exact protocol
behavior is specified by
[SPEC-tau-proto-session-events](../crates/tau-proto/specs/SPEC-tau-proto-session-events.md).
