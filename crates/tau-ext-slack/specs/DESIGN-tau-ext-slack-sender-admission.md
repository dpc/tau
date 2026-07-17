# DESIGN-tau-ext-slack-sender-admission: Sender admission is independent from trigger scope and content trust

Status: confirmed, 2026-07-14, dpc

Strict mode admits only allowlisted Slack-verified humans. Lax mode admits other
verified humans only on static receive routes. It changes sender admission, not
content trust: payloads remain untrusted, with identity and policy typed
separately. Lax never grants dynamic DM linking, agent selection, bridge
commands, or destination authority; dynamic links remain exact allowlisted-user
bindings even in lax mode.

Admission remains live per occurrence; there is no positive identity cache.
Successful-ACK occurrences run through one bounded 64-occurrence serial FIFO, so
blocking `users.info` and bridge-local `chat.postMessage` calls cannot delay later
websocket reads, ACKs, Ping/Pong, reconnect, or shutdown. The worker rechecks
configuration and ingress lifecycle epochs after identity I/O and before effects
or submission. Session shutdown and inactive configuration replacement invalidate
late work; reconnect does not, preserving already-ACKed order.

Before entering that FIFO, a supported event wrapper must match the exact
`auth.test` bot/team installation through `context_team_id` or one unambiguous
authorization record. Top-level event team and actor home team are not
installation authority. Exact human verification and optional bounded display
capture come from one `users.info` snapshot; see
[DESIGN-tau-ext-slack-sender-identity](DESIGN-tau-ext-slack-sender-identity.md).

Each route independently chooses mentions-only or all-message triggers. Outside
DMs, commands require an exact leading authenticated bot mention regardless of
Slack event wrapper. Only an accepted ingress result
activates its pending opaque source-bound reply route and grants no proactive
authority.
