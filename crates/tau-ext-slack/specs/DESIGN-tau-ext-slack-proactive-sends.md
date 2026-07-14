# DESIGN-tau-ext-slack-proactive-sends: Configured proactive transport sends

Status: confirmed, 2026-07-14, dpc

A `conversations` record grants proactive initiation only with
`proactive_send: true`. This is independent from its optional receive permission
and from dynamic DM links. `slack_send` accepts `message` plus exactly one opaque
`reply_to` or configured `destination`; the schema exposes sorted aliases and
verbatim trusted descriptions, never native ids or thread selectors. Agents do
not need `slack_register` for proactive sends; effective role/tool policy remains
the agent authorization layer.

The extension and harness revalidate connection, session, agent/tool,
capability, alias, endpoint, kind/id, and fixed thread. Same-process accepted
retry does not repost. Transcript replay never posts remotely, and Tau does not
claim exactly-once delivery across crashes or ambiguous Slack responses. Prompt
injection can influence a role granted this tool, so destination sets and roles
should remain minimal. App membership is required; `chat:write.public` is not.
