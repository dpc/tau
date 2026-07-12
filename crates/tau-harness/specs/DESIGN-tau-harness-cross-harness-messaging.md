# DESIGN-tau-harness-cross-harness-messaging: Cross-harness agent messages use a dedicated asynchronous RPC

Status: confirmed, 2026-07-12, tau-agent-44tt user approval

The harness-owned `message` tool accepts bare `&<session-id>`, explicit
`&<session-id>/@<agent-id>`, and the legacy exact
`<session-id>/<agent-id>` spelling. A current-session address remains local.
It must not pack sigils or session ids into `AgentId`; protocol and event
payloads carry typed route authority, session, and agent identity separately.

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
loop. The target performs bounded syntax/size and claimed-session checks, and
callback correlation confirms that a cooperative same-UID Tau sender owns the
exact pending route/body binding. This is accidental-misrouting and spend
protection, not an ACL against malicious same-user processes. Callback socket
discovery and I/O use bounded global admission, absolute deadlines, and a
generation-tagged harness command. Disconnect, deadline, or rollover cancels
retained work.

Bare and exact recipient authority are distinct authenticated protocol values;
a capability for one cannot authorize the other. After authentication the
target event loop revalidates the session and policy, then selects one eligible
entrypoint endpoint, preferring idle/pending over running and least-recently
routed then agent id. Busy eligible endpoints are reused rather than causing
fan-out. Successful bare sends return the canonical resolved session/agent and
whether an endpoint was started. If no eligible endpoint exists, only the
entrypoint's explicit `auto_start_role` may be constructed. Admission precedes
creation, pending/live endpoints provide in-process single-flight coalescing, and
busy eligible endpoints remain preferable to fan-out.

Tests should cover the runtime metadata active-session contract, stale/ambiguous
discovery, untrusted peer rejection, target-session and recipient validation,
external prompt/UI labels, sender capability binding, non-blocking receiver-side
authentication, and failure not publishing a sent projection. Focused event-loop
and interception tests cover auth/admission before spend, count/byte/rate
boundaries (including pending endpoints), parked local/remote single-flight, busy
reuse, ordinary tool-capable construction without remote inheritance,
reselect-once, rollover/failure cleanup, and receive-commit ordering. Real
two-harness socket tests cover callback correlation and auto-start acknowledgement
parity.

Outbound lookup and socket work likewise uses 16 non-queued process-wide slots
and one absolute deadline. Runtime lookup counts at most 128 visited entries and
reads at most 16 KiB per candidate before connect/send/response work. A success
projection is impossible until the target's exact receive projection commits.
The acknowledgement continuation is bounded, in-memory, and generation-bound.
Delivery is best-effort at-least-once: a crash after receive commit but before
acknowledgement can lead to a duplicate prompt on retry. The same crash ambiguity
may duplicate agent creation, model work, or spend; no distributed WAL, locator,
restart deduplication, or durable transaction coordinator exists. Live commit-time
authority is revalidated, with at most one bare-route reselection.

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
