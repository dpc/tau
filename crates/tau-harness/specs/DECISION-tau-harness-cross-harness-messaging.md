# DECISION-tau-harness-cross-harness-messaging: Dedicated asynchronous peer messaging

Authority: confirmed, 2026-07-14, user

Cross-harness agent messages use a dedicated typed socket RPC rather than generic
extension event emission. Runtime lookup, socket work, and receiver-side sender
authentication run off the central harness event loop with bounded admission and
deadlines. Sender identity and exact-versus-bare recipient authority remain typed
protocol values rather than being packed into agent IDs.

A sender records success only after the target's exact receive projection commits.
Delivery is nevertheless cooperative same-UID, best-effort at-least-once IPC: a
crash after receive commit but before acknowledgement can duplicate a prompt,
agent creation, model work, or spend on retry. Tau deliberately does not add a
distributed WAL, restart deduplication index, or transaction coordinator for this
path.

The dedicated RPC and callback correlation protect against accidental misrouting
and unintended spend, not malicious same-user processes. Exact routing, bare
entrypoint selection, discovery, admission, limits, and failure behavior are
specified by
[SPEC-tau-harness-peer-routing](SPEC-tau-harness-peer-routing.md) and
[SPEC-tau-harness-peer-discovery](SPEC-tau-harness-peer-discovery.md). The wider
boundary is [ARCH-external-message-boundary](../../../specs/ARCH-external-message-boundary.md).
