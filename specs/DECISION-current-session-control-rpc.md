# DECISION-current-session-control-rpc: Query current session identity from live harness memory

Authority: confirmed, 2026-07-21, dpc

`tau session list` must use a narrow requester-directed
`get_current_session`/`current_session_result` control RPC to obtain each
responsive local harness's current session identity: its current session id and
its absolute canonical startup project root. The harness event loop's in-memory
`current_session_id` and immutable startup `project_root` are authoritative at
request handling time. Runtime socket paths locate candidates only; adjacent
metadata and persisted session journals do not provide listing rows or either
returned field.

The request carries a caller-generated correlation id. The response echoes it
and carries a typed `SessionId` plus the canonical project-root path. It is sent
only to the requesting connection and is neither published nor persisted. The
harness accepts the request only from connections carrying its local UI/control
classification. Socket connections receive that classification when accepted;
`Hello.client_kind` does not grant or revoke this same-UID local control
capability. Extensions and supervised tool/provider connections cannot observe
the response.

This RPC changes the shared harness protocol but does not change persistence,
replay, or extension lifecycle semantics. Protocol compatibility follows the
project's no-backward-compatibility policy.

Implementation authority and bounded runtime discovery are documented in:

- [`crates/tau-proto/specs/ARCH-tau-proto.md`](../crates/tau-proto/specs/ARCH-tau-proto.md)
- [`crates/tau-proto/specs/SPEC-tau-proto-session-events.md`](../crates/tau-proto/specs/SPEC-tau-proto-session-events.md)
- [`crates/tau-harness/specs/ARCH-tau-harness.md`](../crates/tau-harness/specs/ARCH-tau-harness.md)
- [`crates/tau-harness/specs/SPEC-tau-harness-event-processing.md`](../crates/tau-harness/specs/SPEC-tau-harness-event-processing.md)
- [`crates/tau-cli/specs/ARCH-tau-cli.md`](../crates/tau-cli/specs/ARCH-tau-cli.md)
