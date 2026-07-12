# DESIGN-tau-harness-cross-harness-messaging: Cross-harness agent messages use a dedicated asynchronous RPC

Status: confirmed, 2026-07-12, tau-agent-44tt user approval

The harness-owned `message` tool treats `<session-id>/<agent_id>` as an external
address only when the session id differs from the current active session. It
must not pack the session id into `AgentId`; protocol and event payloads carry
session and agent identity separately.

External delivery uses a dedicated socket RPC, not generic `emit`, and the
runtime-dir lookup plus socket round-trip runs off the event loop. The helper
thread reports completion back with a harness command. Sender-side
`agent.message_sent` projections represent confirmed delivery, so lookup/socket
or target validation failure completes the tool with an error without recording a
successful send projection.

Runtime-dir stale cleanup is conservative: failed socket probes must not remove
discovery files while the advertised daemon pid is still live. Dead-pid entries
remain eligible for cleanup where Tau has a safe pid-liveness backend, but a
transient connection failure must not make a running daemon permanently
undiscoverable to external-message lookup or CLI attach.

Receiver-side sender authentication must not block the central harness event
loop. After cheap target validation, callback socket discovery and I/O run on a
helper thread and return a harness command that sends the RPC result and commits
the inbound projection only after the claimed sender authorizes the exact sender,
recipient, kind, and message body fields.

Tests should cover the runtime metadata active-session contract, stale/ambiguous
discovery, untrusted peer rejection, target-session and recipient validation,
external prompt/UI labels, sender capability binding, non-blocking receiver-side
authentication, and failure not publishing a sent projection.

Opt-in discovery, bare entrypoint routing, and its separate auto-start authority
are governed by
[DESIGN-peer-entrypoints](../../../specs/DESIGN-peer-entrypoints.md).

Schema-guided argument repair runs only in the pre-dispatch validation failure
branch. The harness executes a repaired call only after the repaired arguments
pass the same schema validator, emits a non-mandatory notice/log trace for the
local repair, and otherwise preserves the rejection/error/example behavior used
for unrepaired failures.

Testing is split by owner. `tau-config` tests cover file and CLI alias
normalization, keyed rule layering, and tag-pattern parsing/rejection.
`tau-harness` tests cover evaluator ordering, role broad-to-specific overrides,
the built-in policy through the shared evaluator, and prompt-owned snapshot
authorization.
