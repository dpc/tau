# ARCH-tau-ext-slack: tau-ext-slack architecture

External ingress is constrained by [ARCH-external-message-boundary](../../../specs/ARCH-external-message-boundary.md). Conversation policy follows [DESIGN-tau-ext-slack-conversation-policy](DESIGN-tau-ext-slack-conversation-policy.md).

`std-slack` is a disabled-by-default Socket Mode text bridge exposing scoped
`slack_register`, `slack_conversations`, `slack_send`, and separately authorized
default-off `slack_react` tools. Configuration validates one exact
conversation-route list into alias, parent-receive, thread-receive, and
proactive-alias indexes. Routes carry explicit channel/MPIM/DM kind and optional
fixed root. A bounded runtime map holds exact dynamic D-to-U/W links.

Supported Socket events reserve one of 64 process-local outstanding slots before
ACK. A successful local ACK write commits the occurrence to one persistent serial
FIFO that survives websocket reconnects; failed ACKs release the reservation, and
saturation/worker closure does not ACK so Slack may retry after reconnect. The FIFO
owns capacity through the terminal admission outcome and keeps identity checks,
local replies, and ingress submission off the websocket reader while preserving
global successful-ACK order. It is not durable: process death after ACK retains the
existing loss window. Socket events are authorized only after ACK. The bridge rejects malformed,
bot/self, subtype, kind, route, thread, and sender metadata; verifies humans via
`users.info`; applies strict/lax and allowlist-only control; then submits a typed
native route to a route-selected registered agent. `message` and `app_mention`
wrappers normalize leading-mention commands and `(conversation, ts)` create
identity identically. Parent routes share selection across actual threads;
receive-enabled fixed-thread routes and dynamic DMs isolate it.

The harness stamps trust, durably deduplicates, and commits ingress. Only a
protocol-v11 Committed+Active result whose exact first canonical instance,
target, native occurrence, human, conversation/thread, assurance, and policy
match pending state installs source-bound reply/edit/reaction authority. Inactive,
Rejected, orphaned, or mismatched results install nothing. Successful sends install
bounded reaction ownership. Edits and reactions revalidate original/owner,
sender, agent, receive route, thread, capability, and completion. Replay cannot
wake or reactivate a route.
These ownership flows follow
[DESIGN-tau-ext-slack-canonical-reply-selectors](DESIGN-tau-ext-slack-canonical-reply-selectors.md),
[DESIGN-tau-ext-slack-edit-ownership](DESIGN-tau-ext-slack-edit-ownership.md),
and [DESIGN-tau-ext-slack-reaction-ownership](DESIGN-tau-ext-slack-reaction-ownership.md).
The cross-adapter authority contract is
[DESIGN-canonical-transport-ingress](../../../specs/DESIGN-canonical-transport-ingress.md).

`slack_conversations` returns bounded pages of static aliases, operator
descriptions, kinds, scopes, and configured receive/proactive policy; it excludes
native routes, dynamic links, identities, and runtime state.
`slack_send` accepts exactly one opaque reply id or a plain current proactive alias.
It never accepts a native id or thread. The extension and
harness independently revalidate agent, tool, session, capability, route,
endpoint, kind, thread, and completion. MPIM metadata uses `ConversationKind::Group`.
Thread sends use the immutable root and never broadcast replies.
Agent-authored reply and proactive text is unchanged by default. The
presentation-only `prefix_agent_id` setting may add the legacy `[agent-id] `
prefix after message-size validation; it does not alter authorization, routing,
thread selection, or send cardinality.
See [DESIGN-tau-ext-slack-proactive-sends](DESIGN-tau-ext-slack-proactive-sends.md)
and [DESIGN-tau-ext-slack-conversation-discovery](DESIGN-tau-ext-slack-conversation-discovery.md)
and [DESIGN-tau-ext-slack-immutable-thread-destinations](DESIGN-tau-ext-slack-immutable-thread-destinations.md).

Runtime links, selections, registrations, reply routes, post ownership, and
worker state clear on restart. Durable create identity permits Slack retry after
restart to deduplicate, but historical duplicates are Inactive and restore no
private authority. Same-process accepted sends do not
repost, without claiming crash-safe exactly-once delivery.

Configuration has a monotonic freeze latch. Successful auth/socket preflight
freezes the worker snapshot before activation; otherwise the first fully
authorized post or reaction freezes under the state lock before Slack I/O. Invalid calls and
failed synchronous preflight do not freeze. Invalid pre-freeze replacement
clears inactive authority and capability metadata.

Strict mode admits allowlisted verified humans. Lax additionally admits verified
humans only on static routes; it never grants control or dynamic linking.
Dynamic DMs always remain exact allowlisted-user-bound. External text remains
`UntrustedExternal`. Native ids remain durable authorization/audit metadata;
only stable aliases, trusted operator descriptions, and factual configured
kind/scope/receive/proactive policy are discoverable.
Admission authority follows
[DESIGN-tau-ext-slack-sender-admission](DESIGN-tau-ext-slack-sender-admission.md).

Socket lifecycle, rejection logging, diagnostics, token redaction, HTTPS/WSS
requirements, and shutdown are fail-closed and identifier-free as described in
the [README](../README.md). Payload-free `TRACE` latency markers use only monotonic
durations, bounded classes/depth buckets, connection generations, and process-local
occurrence/request ordinals; they never enter the durable event protocol.
The decision and privacy contract are recorded in
[DESIGN-tau-ext-slack-latency-observability](DESIGN-tau-ext-slack-latency-observability.md).

## Agent-invoked reactions

The disabled-by-default `slack_react` (`slack:react`) tool mutates only exact,
commit-accepted Tau-issued message references. Native Slack identifiers remain
private. Targets, same-agent reaction ownership, in-flight reservations, and
ToolCallId attempts are bounded runtime state and are revalidated against the
current extension, capability, agent, session, route, dynamic link, and config
on every call. `slack_send` returns a collision-resistant opaque `message_ref`
that activates only after accepted durable completion. See
[DESIGN-tau-ext-slack-agent-reactions](DESIGN-tau-ext-slack-agent-reactions.md).
