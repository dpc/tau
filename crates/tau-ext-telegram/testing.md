# Telegram testing

Tests are hermetic. Use `FakeClient` for Bot API behavior and loopback Unix
sockets for gateway/sidecar behavior; never require credentials, public network
access, or wall-clock sleeps.
Concurrency regressions use fixture-controlled barriers to force the relevant
interleaving rather than depending on scheduler timing or stress frequency.

Extension tests own Telegram admission, routing, offset and lease behavior,
duplicate suppression, transient `message.*_reported` metadata, and serialized
report-before-tool-result submission. Local-poll tests force ordered mixed
checkpoint, exact canonical-echo, missing-echo replay, out-of-order echo,
reconnect/backlog-drain, and non-routed duplicate-reply interleavings without
wall-clock races. Cover both direct polling and gateway client paths. Gateway
response conformance uses the real gateway client to cover
the 32-record maximum response prefix, both send and heartbeat producers, and
repeated exact replay before ACK. Gateway tests force mixed-prefix ordering,
restart/re-registration replay, ACK progress independent of long polling,
late ACK after route retirement, mismatched-route rejection, dropped ACK
responses, bounded retry state,
corrupt typed IDs, concurrent update/ACK commit, and deterministic routed/ACK
state-save cuts at write, file sync, rename, and parent-directory sync. These
assert rollback before installation and fail-stop restart recovery after
installation.
Subprocess tests exercise the stable exit contract with loopback HTTP fixtures:
help/usage/configuration, active webhook and lock contention, the complete
preflight status classification, refused transport, corrupt filesystem state,
and post-preflight `getUpdates` HTTP 409. Response bodies deliberately include
the fixture token so every emitted failure class also checks redaction.
Focused serialization tests cover the exact
newline-inclusive 65,536-byte boundary, JSON escaping, and multibyte UTF-8.

Gateway-client supervisor fixtures replace loopback Unix listeners and use
socket requests plus condition-variable notifications as synchronization. They
cover an initially absent gateway, fresh hello and exact route reannouncement
after restart, recovered sends, disconnected fail-closed behavior, bounded
backoff, and synchronous stale-configuration cancellation without public
network access or timing sleeps. A response barrier proves reconfiguration
waits for worker retirement, and a saturated, unaccepted Unix-listener backlog
proves connect cancellation remains bounded.

Harness tests own report authority and interception, downstream canonical fact
durability, live order, replay, transcript projection, and model wake. Do not
duplicate those semantics in this crate or treat protocol-writer flush as a
canonical-commit acknowledgement.
