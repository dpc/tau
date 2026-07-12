# DESIGN-tau-ext-slack-sender-admission: Sender admission is independent from trigger scope and content trust

Status: unconfirmed

Strict mode is the default and admits only allowlisted verified humans. Lax mode accepts the increased prompt-injection exposure of other Slack-verified non-bot humans only in configured channels or an already-linked DM. This changes sender admission, not content trust: payloads remain untrusted, and identity plus `Allowlisted`/`LaxPermitted` policy are typed separately. Lax senders cannot link DMs or use agent-selection and bridge-control commands. Accepted ingress activates only an opaque source-bound reply route for the authenticated actor, conversation, and thread; it grants no arbitrary destination selection. Mentions-only/all-messages trigger scope is orthogonal and must preserve these sender, conversation, control, and route invariants.
