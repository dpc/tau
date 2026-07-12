# SPEC-tau-harness-peer-routing: Best-effort typed peer routing

The harness-owned `message` tool accepts bare `&<session-id>`, typed exact
`&<session-id>/@<agent-id>`, and legacy exact `<session-id>/<agent-id>`
addresses. Bare and exact authority are distinct protocol values. Bare routing
selects exactly one eligible loaded or pending entrypoint agent, preferring idle
over running and least-recently-routed then agent id. Busy eligible agents are
reused. Auto-start is inactive in this phase, so every success reports
`started: false` and absence of an eligible endpoint fails without spawning.

Remote routing uses cooperative same-UID Tau IPC. Callback correlation proves
that the claimed sender harness owns a matching live pending request and binds
sender session/agent, recipient authority, kind, and body. It prevents accidental
misrouting and unintended spend; it is not an ACL against malicious same-user
processes. Peer text remains escaped agent-authored model input, never a
harness/system instruction.

Runtime lookup visits at most 128 entries, reads at most 16 KiB of regular
metadata per candidate, and fails closed when its scan/deadline cannot prove a
unique live claimant. Outbound and inbound socket jobs use bounded global
admission and absolute deadlines. Potentially blocking runtime lookup is
isolated behind a separate 16-job non-queued lease; a stalled storage worker
retains that lease after caller timeout so repeated stalls remain bounded. A
64 KiB message limit bounds accepted peer text. Disconnect and session rollover cancel live work, and generation-tagged
completions cannot enter a replacement session.

After validation and endpoint selection, the target enqueues the exact
`AgentMessageReceived` projection but does not acknowledge it. A bounded
in-memory, generation-bound continuation acknowledges only from the
post-persistence commit hook. Interception rejection, persistence failure,
target disappearance, disconnect, or rollover before commit fails or removes
the continuation without success. Only confirmed acknowledgement permits the
sender's `AgentMessageSent` projection.

Delivery is best-effort at-least-once, not distributed exactly-once. A crash or
transport loss after receive commit but before acknowledgement is indeterminate;
retry may duplicate the prompt. There is no cross-session WAL, restart
resumption, or deduplication index in this phase.

Policy and trust are governed by
[DESIGN-peer-entrypoints](../../../specs/DESIGN-peer-entrypoints.md) and
[DESIGN-tau-harness-cross-harness-messaging](DESIGN-tau-harness-cross-harness-messaging.md).
