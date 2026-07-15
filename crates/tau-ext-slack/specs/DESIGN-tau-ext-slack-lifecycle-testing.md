# DESIGN-tau-ext-slack-lifecycle-testing: Slack lifecycle tests use local fakes

Status: inferred

Slack lifecycle behavior is tested without live Slack credentials. Unit tests use
fake `SlackClient` implementations for Web API calls and loopback websocket
servers for Socket Mode behavior. Production Socket Mode URLs still require
`wss`, while tests may use `ws://127.0.0.1` so shutdown, reconnect, framing, and
ack behavior can be exercised deterministically without external network access.
Admission queue unit tests cover reservation release, the 64-occurrence
queued-plus-in-flight bound, FIFO commit order, and closure draining. A loopback
websocket plus blocking identity barrier proves that a later envelope ACK, Pong,
and shutdown stay responsive; focused lifecycle and closed-writer tests prove late
identity completion and failed output cannot create ingress.

Regression tests should prefer this fake-client and loopback approach over real
Slack workspaces. When testing shutdown paths, drive the worker to the blocked
state being exercised, request shutdown through the shared signal, and bound the
post-request wait below any removed polling interval so polling regressions fail.
