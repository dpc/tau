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
Focused serialization tests cover the exact
newline-inclusive 65,536-byte boundary, JSON escaping, and multibyte UTF-8.

Harness tests own report authority and interception, downstream canonical fact
durability, live order, replay, transcript projection, and model wake. Do not
duplicate those semantics in this crate or treat protocol-writer flush as a
canonical-commit acknowledgement.
