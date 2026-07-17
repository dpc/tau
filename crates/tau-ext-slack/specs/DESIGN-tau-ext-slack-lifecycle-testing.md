# DESIGN-tau-ext-slack-lifecycle-testing: Slack lifecycle tests use local fakes

Status: inferred; transport-RPC portions superseded 2026-07-17 by
[DESIGN-extension-published-message-facts](../../../specs/DESIGN-extension-published-message-facts.md)

Slack lifecycle behavior is tested without live credentials. Unit tests use fake
`SlackClient` implementations for Web API calls and loopback websocket servers
for Socket Mode. Production URLs still require `wss`; tests may use loopback
`ws` to exercise shutdown, reconnect, framing, and ACK behavior.

Admission tests cover reservation release, the 64-occurrence bound, FIFO order,
and closure draining. Lifecycle tests cover native duplicate suppression,
identity failure, installation mismatch, route/config/session retirement, and
writer closure. Direct message-fact tests cover delivered/edit/reaction/delete
target identity, incoming and outgoing delete cleanup, sent-before-result frame
order, and same-call replay without reposting.

Send tests inject typed post outcomes and an event-driven scheduler rather than
sleeping. Focused coverage includes the absolute retry horizon, lifecycle
cancellation, active-worker admission, writer failure, and stable replay.

Reaction tests cover strict arguments, source/configured target authorization,
same-agent add/remove ownership, idempotency outcomes, ambiguous failure,
deletion cleanup, late completion, ownership-capacity rejection, and exact HTTP wire shape.
Private native IDs must never appear as accepted route arguments.

Full workspace `selfci` owns compile, documentation, clippy, test/coverage, and
CRAP-score regression gates.
