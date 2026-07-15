# DESIGN-tau-ext-slack-proactive-sends: Configured proactive transport sends

Status: confirmed, 2026-07-14, dpc

A `conversations` record grants proactive initiation only with
`proactive_send: true`. This is independent from its optional receive permission
and from dynamic DM links. `slack_send` accepts `message` plus exactly one opaque
`reply_to` or configured `destination`; its compact fixed schema uses a validated
plain alias and exposes no configured aliases or descriptions. The separately
authorized `slack_conversations` tool discovers model-facing route policy, never
native ids or thread selectors. The alias is resolved and revalidated against
current configuration at send time; no caller-supplied discovery snapshot token
is required. The resolved current
configuration is then frozen in the accepted send intent. Same-alias reuse is
operator responsibility. Agents do
not need `slack_register` for proactive sends; effective role/tool policy remains
the agent authorization layer.

The extension and harness revalidate connection, session, agent/tool,
capability, alias, endpoint, kind/id, and fixed thread. Delivery follows
[DESIGN-tau-ext-slack-send-delivery](DESIGN-tau-ext-slack-send-delivery.md):
one initial attempt plus at most one byte-identical retry, with a non-evicting
same-session replay ledger and cancellation before stale retry/completion.
Ambiguous retry can leave one or two copies. Transcript replay never posts
remotely, and Tau has no durable outbox, restart-spanning idempotency, or
exactly-once claim. Prompt
injection can influence a role granted this tool, so destination sets and roles
should remain minimal. App membership is required; `chat:write.public` is not.
`mention_source_user` remains omitted or false for proactive sends because no
authenticated source route exists. When no worker has observed the current
installation, a read-only `auth.test` preflight binds the exact bot and team
before the send is reserved.
