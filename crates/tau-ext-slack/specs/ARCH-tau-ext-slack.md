# ARCH-tau-ext-slack: tau-ext-slack architecture

External ingress is constrained by [ARCH-external-message-boundary](../../../specs/ARCH-external-message-boundary.md). Conversation policy follows [DESIGN-tau-ext-slack-conversation-policy](DESIGN-tau-ext-slack-conversation-policy.md).

`std-slack` is a disabled-by-default Socket Mode text bridge exposing scoped
`slack_register` and `slack_send` tools. Configuration validates one exact
conversation-route list into alias, parent-receive, thread-receive, and
proactive-alias indexes. Routes carry explicit channel/MPIM/DM kind and optional
fixed root. A bounded runtime map holds exact dynamic D-to-U/W links.

Socket events are acked before authorization. The bridge rejects malformed,
bot/self, subtype, kind, route, thread, and sender metadata; verifies humans via
`users.info`; applies strict/lax and allowlist-only control; then submits a typed
native route to a route-selected registered agent. `message` and `app_mention`
wrappers normalize leading-mention commands and `(conversation, ts)` create
identity identically. Parent routes share selection across actual threads;
receive-enabled fixed-thread routes and dynamic DMs isolate it.

The harness stamps trust, durably deduplicates, and commits ingress. Only an
accepted/identical result installs the canonical source-bound reply route. Create
results also install bounded native edit ownership. Successful sends install
bounded reaction ownership. Edits and reactions revalidate original/owner,
sender, agent, receive route, thread, capability, and completion. Replay cannot
wake or reactivate a route.
These ownership flows follow
[DESIGN-tau-ext-slack-canonical-reply-selectors](DESIGN-tau-ext-slack-canonical-reply-selectors.md),
[DESIGN-tau-ext-slack-edit-ownership](DESIGN-tau-ext-slack-edit-ownership.md),
and [DESIGN-tau-ext-slack-reaction-ownership](DESIGN-tau-ext-slack-reaction-ownership.md).

`slack_send` accepts exactly one opaque reply id or currently advertised
proactive alias. It never accepts a native id or thread. The extension and
harness independently revalidate agent, tool, session, capability, route,
endpoint, kind, thread, and completion. MPIM metadata uses `ConversationKind::Group`.
Thread sends use the immutable root and never broadcast replies.
See [DESIGN-tau-ext-slack-proactive-sends](DESIGN-tau-ext-slack-proactive-sends.md)
and [DESIGN-tau-ext-slack-immutable-thread-destinations](DESIGN-tau-ext-slack-immutable-thread-destinations.md).

Runtime links, selections, registrations, reply routes, post ownership, and
worker state clear on restart. Durable create identity permits Slack retry after
restart to deduplicate and restore edit ownership; edits need that create retry,
and old post reactions remain unavailable. Same-process accepted sends do not
repost, without claiming crash-safe exactly-once delivery.

Configuration has a monotonic freeze latch. Successful auth/socket preflight
freezes the worker snapshot before activation; otherwise the first fully
authorized post freezes under the state lock before Slack I/O. Invalid sends and
failed synchronous preflight do not freeze. Invalid pre-freeze replacement
clears inactive authority and capability metadata.

Strict mode admits allowlisted verified humans. Lax additionally admits verified
humans only on static routes; it never grants control or dynamic linking.
Dynamic DMs always remain exact allowlisted-user-bound. External text remains
`UntrustedExternal`. Native ids remain durable authorization/audit metadata;
only stable aliases and trusted operator descriptions are model-visible.
Admission authority follows
[DESIGN-tau-ext-slack-sender-admission](DESIGN-tau-ext-slack-sender-admission.md).

Socket lifecycle, rejection logging, diagnostics, token redaction, HTTPS/WSS
requirements, and shutdown are fail-closed and identifier-free as described in
the [README](../README.md).
