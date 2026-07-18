# ARCH-tau-ext-slack: tau-ext-slack architecture

External messages follow
[ARCH-external-message-boundary](../../../specs/ARCH-external-message-boundary.md)
and the extension-published fact interface in
[DESIGN-extension-published-message-facts](../../../specs/DESIGN-extension-published-message-facts.md).
Conversation policy follows
[DESIGN-tau-ext-slack-conversation-policy](DESIGN-tau-ext-slack-conversation-policy.md).

`std-slack` is a disabled-by-default Socket Mode text bridge exposing scoped
`slack_register`, `slack_conversations`, `slack_send`, and separately authorized
default-off `slack_react` tools. Configuration validates one exact
conversation-route list into alias, parent-receive, thread-receive, and proactive
alias indexes. Routes carry explicit channel/MPIM/DM kind and an optional fixed
thread root. A bounded runtime map holds exact dynamic D-to-U/W links.

Each configured extension instance is intended for one receiving Tau agent at a
time. Multi-agent sharing or retargeting is ad hoc best-effort behavior and does
not provide exact cross-agent routing, permanent ownership, once-only delivery,
or cross-agent deduplication.

## Receive and fact publication

Supported Socket events reserve one of 64 process-local outstanding slots before
ACK. A successful local ACK write admits the occurrence to one persistent
serial FIFO that survives websocket reconnects. Failed ACKs release the
reservation; saturation or worker closure does not ACK so Slack may retry after
reconnect. The FIFO is not durable, so process death after ACK retains the
existing loss window.

Startup and reconnect bind the exact `auth.test` bot U/W and installing T
workspace. Socket events are authorized only when the wrapper proves that
installation through exact `context_team_id` or one unambiguous matching
authorization. The bridge rejects malformed, bot/self, subtype, kind, route,
thread, and sender metadata; verifies humans through `users.info`; applies
strict/lax admission and allowlist-only control; and then selects a registered
Tau agent through extension-local routing.

Admitted creates, edits, deletes, and reactions publish ordinary immutable
`message.delivered`, `message.edited`, `message.deleted`,
`message.reaction_added`, or `message.reaction_removed` facts. Slack derives
opaque fact IDs from native channel/message coordinates and uses
`MessageFactRef` for later operations on the same logical message. The harness
stamps the configured extension publisher and persists the fact before prompt
projection. There is no Slack-specific ingress acknowledgement or harness
transport-registration state.

Slack installs and revalidates reply, edit, and reaction routes locally. A
locally written create establishes source reply/edit authority keyed by its
`MessageFactId`; a locally written outgoing send establishes posted-message reaction
authority keyed by its sent fact ID. Delete publication revokes the matching
local source/reply/reaction target state. These maps are bounded, process-local,
agent- and lifecycle-scoped, and never reconstructed by fact replay.

The extension drops recently repeated native occurrence IDs with a bounded
4,096-entry process-local FIFO set. Cache eviction, races, or restart may
duplicate publication. Generic infrastructure does not deduplicate or resolve
message facts.

Exact U/W identity remains extension-local authority. Model context receives an
installation-scoped opaque sender reference, an honest optional authentication
outcome, and a bounded untrusted display. Optional operator sender aliases are
also presentation-only and do not affect admission, routing, reply, reaction, or
mention authority. See
[DECISION-common-external-message-envelope](../../../specs/DECISION-common-external-message-envelope.md).

## Sending

`slack_conversations` returns bounded pages of static aliases, operator
descriptions, kinds, scopes, and configured receive/proactive policy. It excludes
native routes, dynamic links, identities, and runtime state.

`slack_send` accepts exactly one Tau-issued local reply selector or a current
proactive alias; it never accepts native Slack conversation IDs or thread
timestamps. The extension revalidates the current agent, tool, session, route,
installation, lifecycle, and configuration. Accepted calls freeze their route,
authority snapshot, and final wire body in a bounded process/session ledger
before I/O.

At most 64 active delivery workers run HTTP and notification-driven retry waits
off tau-client's serialized reader. A live per-channel FIFO serializes actual
attempts. One initial attempt plus at most one byte-identical retry provides
process/session at-least-once delivery: an ambiguous first attempt followed by
success can leave one or two Slack copies. There is no durable outbox,
`client_msg_id`, restart guarantee, remote/local transaction, or exactly-once
claim.

After Slack reports success, the extension writes `message.sent` and then the
ordinary `tool.result` through one serialized local write-and-flush gate. This
preserves frame order but does not acknowledge a harness commit. The result's
`message_ref` becomes local reaction authority keyed to the sent fact ID. Its
documented `slack-message:<opaque-digest>` representation exposes no native
coordinates and is accepted only when it resolves to an exact retained target.
Same-call replay returns the retained stable result without posting again or
publishing another sent fact. Conflicting call-ID reuse fails, while a new call
ID is new intent.

Agent-authored text is unchanged by default. The presentation-only
`prefix_agent_id` setting may add the legacy `[agent-id] ` prefix. Raw `<@`,
`<!`, and `<#` controls are rejected. The default-false reply-only
`mention_source_user` option can prepend only the exact verified human bound to
the selected live reply route.

## Lifecycle and safety

Reconnect must match both halves of the established bot/workspace pair. A
changed, incomplete, or malformed observation permanently retires
installation-scoped routes, links, ownership, workers, and publication authority
until restart. Configuration freezes after successful worker preflight or before
the first authorized post or reaction API attempt.

Runtime links, selections, registrations, reply/edit routes, posted-message
ownership, received IDs, and send workers clear on restart. Disconnect, session
rollover, agent unload, route replacement, and tool loss retire the applicable
authority. Already-started synchronous HTTP may outlive `run` only through its
bounded request timeout; retired workers cannot retry or restore local state.

Strict mode admits allowlisted verified humans. Lax mode additionally admits
verified humans on static receive routes, but never grants control or dynamic DM
linking. External text and metadata remain untrusted. Logs and notices use
bounded closed outcome categories and never expose tokens, URLs, response
bodies, message text, or native identifiers other than documented message fact IDs.

The disabled-by-default `slack_react` tool accepts only exact retained Tau-issued
fact references from locally written incoming facts or successful `slack_send` results.
Targets, same-agent reaction ownership, in-flight reservations, and tool-call
attempts are bounded runtime state. Adds establish ownership only after
unambiguous Slack success; removes require that ownership. Ambiguous effects are
not adopted.
