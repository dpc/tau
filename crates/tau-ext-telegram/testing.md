# Telegram testing

Tests are hermetic. Use `FakeClient` for Bot API behavior and loopback Unix
sockets for gateway/sidecar behavior; never require credentials, public network
access, or wall-clock sleeps.

Extension tests own Telegram admission, routing, offset and lease behavior,
duplicate suppression, transient `message.*_reported` metadata, and serialized
report-before-tool-result submission. Cover both direct polling and gateway
client paths.

Harness tests own report authority and interception, downstream canonical fact
durability, live order, replay, transcript projection, and model wake. Do not
duplicate those semantics in this crate or treat protocol-writer flush as a
canonical-commit acknowledgement.
