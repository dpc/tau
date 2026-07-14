# DESIGN-tau-ext-slack-sender-admission: Sender admission is independent from trigger scope and content trust

Status: confirmed, 2026-07-14, dpc

Strict mode admits only allowlisted Slack-verified humans. Lax mode admits other
verified humans only on static receive routes. It changes sender admission, not
content trust: payloads remain untrusted, with identity and policy typed
separately. Lax never grants dynamic DM linking, agent selection, bridge
commands, or destination authority; dynamic links remain exact allowlisted-user
bindings even in lax mode.

Each route independently chooses mentions-only or all-message triggers. Outside
DMs, commands require an exact leading authenticated bot mention regardless of
Slack event wrapper. Accepted ingress activates only its opaque source-bound
reply route and grants no proactive authority.
