# DESIGN-tau-ext-slack-lifecycle-testing: Slack lifecycle tests use local fakes

Status: inferred

Protocol-consumer fixtures cover Active and fail-closed Inactive, Rejected,
orphaned, foreign-instance, and mismatched canonical authority. Only exact
Active may install reply, edit, or reaction state, and presentation-only retries
use the first canonical snapshot.

Slack lifecycle behavior is tested without live Slack credentials. Unit tests use
fake `SlackClient` implementations for Web API calls and loopback websocket
servers for Socket Mode behavior. Production Socket Mode URLs still require
`wss`, while tests may use `ws://127.0.0.1` so shutdown, reconnect, framing, and
ack behavior can be exercised deterministically without external network access.
Admission queue unit tests cover reservation release, the 64-occurrence
queued-plus-in-flight bound, FIFO commit order, and closure draining. A loopback
websocket plus blocking identity barrier proves that a later envelope ACK, Pong,
and shutdown stay responsive; focused lifecycle and closed-writer tests prove late
identity completion and failed output cannot create ingress. Send-delivery tests
hold initial/retry waits across unrelated wakes, retire reserved work at
Disconnect/EOF, enforce live same-channel FIFO and actual-start retry horizons,
and prove initial/replayed completion writer failure retires later outbound I/O.

Regression tests should prefer this fake-client and loopback approach over real
Slack workspaces. When testing shutdown paths, drive the worker to the blocked
state being exercised, request shutdown through the shared signal, and bound the
post-request wait below any removed polling interval so polling regressions fail.

Send-delivery tests inject typed post outcomes and an event-driven scheduler.
They never sleep: barriers hold initial HTTP or retry waiting while a staged real
tau-client reader processes later tools, unregister, and session shutdown.
Fixtures assert exact frozen-body reuse, initial-plus-one budgeting,
Retry-After/jitter bounds, lifecycle cancellation, terminal ledger replay, and
one-or-two-copy ambiguity. Provider privacy fixtures use hostile bodies, tokens,
native ids, mentions, newlines, and bidi sentinels and assert that only closed
categories reach displays or protocol output.

Identity fixtures cover missing/malformed bot/team responses, exact event-wrapper
installation matching without top-level actor-team authority, reconnect pair
mismatch/partial-state failure, no-cache per-occurrence lookup, alias mutation
across a duplicate, safe display filtering, and first-canonical presentation.
Mention fixtures cover explicit and omitted false behavior, reply-only targeting,
raw entity controls, exact generated wire text, and stale-installation failure.
CLI fixtures cover compact reaction Add/Remove facts, actor, exact name, hostile
Unicode, grapheme truncation, and final byte/column caps.
