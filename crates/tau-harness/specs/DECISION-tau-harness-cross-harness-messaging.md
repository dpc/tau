# DECISION-tau-harness-cross-harness-messaging: Dedicated asynchronous peer messaging

Authority: confirmed, 2026-07-14, user

Cross-harness agent messages use a dedicated typed socket RPC rather than generic
extension event emission. Runtime lookup, socket work, and receiver-side sender
authentication run off the central harness event loop with bounded admission and
deadlines. Sender identity and exact-versus-bare recipient authority remain typed
protocol values rather than being packed into agent IDs.

A sender records success only after the target commits its receive projection.
Delivery is nevertheless cooperative same-UID, best-effort at-least-once IPC; a
crash after commit but before acknowledgement can duplicate work. Tau accepts
that ambiguity rather than adding a distributed WAL, restart deduplication, or a
transaction coordinator.

The RPC protects against accidental misrouting, not malicious same-user
processes. Exact behavior is specified by
[SPEC-tau-harness-peer-routing](SPEC-tau-harness-peer-routing.md) and
[SPEC-tau-harness-peer-discovery](SPEC-tau-harness-peer-discovery.md).
Receive-commit projection, runtime activation, replay, and provider-context
cardinality are governed by
[DECISION-agent-message-transcript-projection](../../../specs/DECISION-agent-message-transcript-projection.md)
and specified end to end by
[SPEC-agent-message-delivery](../../../specs/SPEC-agent-message-delivery.md).
