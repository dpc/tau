# Telegram testing

Tests are hermetic. Use `FakeClient` for Bot API behavior and loopback Unix
sockets for gateway/sidecar behavior; never require credentials, public network
access, or wall-clock sleeps.

Extension tests own Telegram admission, routing, offset and lease behavior,
duplicate suppression, transient `message.*_reported` metadata, and serialized
report-before-tool-result submission. Cover both direct polling and gateway
client paths. Gateway response conformance uses the real gateway client to cover
the 32-record maximum queue, both send and heartbeat producers, and repeated
ordered bounded drain. Focused serialization tests cover the exact
newline-inclusive 65,536-byte boundary, JSON escaping, and multibyte UTF-8.

Harness tests own report authority and interception, downstream canonical fact
durability, live order, replay, transcript projection, and model wake. Do not
duplicate those semantics in this crate or treat protocol-writer flush as a
canonical-commit acknowledgement.
