# DECISION-tau-ext-slack-sender-admission: Sender admission is independent from trigger scope and content trust

Authority: confirmed, 2026-07-14, dpc

Strict mode admits only allowlisted Slack-verified humans. Lax mode admits other
verified humans only on static receive routes. It changes sender admission, not
content trust: payloads remain untrusted, with identity and policy typed
separately. Lax never grants dynamic DM linking, agent selection, bridge
commands, or destination authority; dynamic links remain exact allowlisted-user
bindings even in lax mode.

Separating admission, trigger scope, and body trust lets operators widen one
static ingress route without turning display or untrusted content into identity
or control authority. Exact verification, FIFO, lifecycle, trigger, and
report-submission behavior is [SPEC-tau-ext-slack-ingress](SPEC-tau-ext-slack-ingress.md).
