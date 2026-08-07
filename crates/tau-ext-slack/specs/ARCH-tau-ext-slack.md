# ARCH-tau-ext-slack: tau-ext-slack architecture

External messages follow
[ARCH-external-message-boundary](../../../specs/ARCH-external-message-boundary.md)
and the extension-report/canonical-fact interface in
[SPEC-external-message-reports-and-facts](../../../specs/SPEC-external-message-reports-and-facts.md).
Exact routing, ingress, and mutation behavior is specified by
[SPEC-tau-ext-slack-conversation-routing](SPEC-tau-ext-slack-conversation-routing.md),
[SPEC-tau-ext-slack-ingress](SPEC-tau-ext-slack-ingress.md), and
[SPEC-tau-ext-slack-message-mutations](SPEC-tau-ext-slack-message-mutations.md).
Focused boundaries are
[SPEC-tau-ext-slack-send-delivery](SPEC-tau-ext-slack-send-delivery.md),
[SPEC-tau-ext-slack-source-mentions](SPEC-tau-ext-slack-source-mentions.md),
[SPEC-tau-ext-slack-agent-reactions](SPEC-tau-ext-slack-agent-reactions.md), and
[SPEC-tau-ext-slack-latency-observability](SPEC-tau-ext-slack-latency-observability.md).

`std-slack` is a disabled-by-default Socket Mode text bridge exposing scoped
`slack_register`, `slack_conversations`, `slack_send`, and separately authorized
default-off `slack_react` tools. Configuration validates one exact
conversation-route list into alias, parent-receive, thread-receive, and proactive
alias indexes. Routes carry explicit channel/MPIM/DM kind and an optional fixed
thread root. A bounded runtime map holds exact dynamic D-to-U/W links.

Each configured extension instance is intended for one receiving Tau agent at a
time. Multi-agent sharing or retargeting is ad hoc best-effort behavior and does
not provide exact cross-agent routing, permanent ownership, once-only delivery,
or cross-agent deduplication. Dependable deployments use separate configured
instances for separate receiving agents.

## Receive and report submission

Supported Socket events reserve one of 64 process-local outstanding slots before
ACK. A successful local ACK write admits the occurrence to one persistent
serial FIFO that survives websocket reconnects. Failed ACKs release the
reservation; saturation or worker closure does not ACK so Slack may retry after
reconnect. Report-bearing work retains its reserved slot through exact canonical
confirmation; commands and terminal rejections release theirs immediately.
Missing canonical echoes therefore stop later ACK admission at 64 outstanding
occurrences instead of silently advancing. The FIFO is not durable, so process
death after ACK retains the existing loss window.

Startup and reconnect bind the exact `auth.test` bot U/W and installing T
workspace. Socket events are authorized only when the wrapper proves that
installation through exact `context_team_id` or one unambiguous matching
authorization. The bridge rejects malformed, bot/self, subtype, kind, route,
thread, and sender metadata; verifies humans through `users.info`; applies
strict/lax admission and allowlist-only control; and then selects a registered
Tau agent through extension-local routing.

One process-lifetime worker thread owns sequential Socket Mode connections.
`worker_started` records that this thread was launched and never offers an
in-process restart path. `worker_online` means only that the current connection
observed Slack's `hello`; a connection guard clears it on every return path.
The first failed startup or reconnect sets a process-lifetime notice latch and
emits one bounded categorical warning. Later failures remain visible in local
logs without repeating the warning.

The worker actively probes each WebSocket after 10 seconds and every 10 seconds
thereafter. A resettable deadline expires independently 40 seconds after the
latest Pong; no other frame refreshes it. Ping, Pong, and ACK writes race both
shutdown and that same deadline so outbound backpressure cannot pin the reader.
Deadline or shutdown drops the WebSocket rather than attempting a potentially
blocked close write. Stale connections reconnect through the same installation
validation path rather than waiting indefinitely for another inbound frame.

Admitted creates, edits, deletes, and reactions submit ordinary `persist=false`
`message.delivered_reported`, `message.edited_reported`, `message.deleted_reported`,
`message.reaction_added_reported`, or `message.reaction_removed_reported` reports. Slack derives
opaque fact IDs from native channel/message coordinates and uses
`MessageFactRef` for later operations on the same logical message. The harness
stamps the configured extension publisher and persists the canonical fact before prompt
projection. Each report also carries a stable opaque Slack report occurrence ID.
The extension retains the report until the same canonical event type, target
agent, configured publisher, message identity, and report ID return on its live
post-commit downpath. This asynchronous observation is not a synchronous protocol
ACK or harness transport-registration state.

Slack installs and revalidates reply, edit, and reaction routes locally. A
canonically confirmed create report establishes source reply authority under its
Tau-issued `MessageFactId`, while edit lookup uses the private native `(channel,
ts)` tuple bound to that report. A canonically confirmed outgoing send report establishes posted-message
reaction authority under its sent report ID while retaining private native
coordinates for API routing. Delete-report submission revokes matching local
source/reply/edit/reaction state immediately, but retains the report and admission
slot until canonical confirmation. These maps are bounded, process-local, agent-
and lifecycle-scoped, and never reconstructed by canonical-fact replay. This
Slack-local authority is never installed merely because a local report frame was
flushed.

The extension drops recently repeated native occurrence IDs with a bounded
4,096-entry process-local FIFO set. A duplicate whose report is still pending
replays the exact report; after canonical confirmation the same occurrence is
dropped. Recording precedes identity lookup, local effects, and report
construction; failure before pending installation remains suppressed until cache
eviction or restart. Cache eviction, races, or restart may duplicate report submission.
Generic infrastructure does not deduplicate or resolve message facts.

Exact U/W identity remains extension-local authority. Model context receives an
installation-scoped opaque sender reference, an honest optional authentication
outcome, and a bounded untrusted display. Optional operator sender aliases are
also presentation-only and do not affect admission, routing, reply, reaction, or
mention authority. See
[SPEC-external-message-reports-and-facts](../../../specs/SPEC-external-message-reports-and-facts.md).

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

After Slack reports success, the extension writes `message.sent_reported` and then
`tool.result_reported` through one serialized local write-and-flush gate. This
preserves frame order but does not acknowledge a harness commit. The typed result
remains the explicit tool result and Slack's HTTP response remains remote-effect
authority. The sent report stays pending until the configured publisher observes
the same event type, target agent, configured publisher, and stable message ID in
its canonical `message.sent` fact on the post-commit live downpath. Only that echo
completes the ledger entry and makes the result's
`message_ref` local reaction authority. A lost echo retains pending state and may
cause replay after lifecycle loss; it cannot claim canonical publication. Its
documented `slack-message:<opaque-digest>` representation exposes no native
coordinates and is accepted only when it resolves to an exact retained target.
Same-call replay coalesces while canonical confirmation is pending, then returns
the retained stable result without posting again or submitting another sent
report. Conflicting call-ID reuse fails, while a new call ID is new intent.

Agent-authored text is unchanged by default. The presentation-only
`prefix_agent_id` setting may add the legacy `[agent-id] ` prefix. Raw `<@`,
`<!`, and `<#` controls are rejected. The default-false reply-only
`mention_source_user` option can prepend only the exact verified human bound to
the selected live reply route.

## Lifecycle and safety

Reconnect must match both halves of the established bot/workspace pair. A
changed, incomplete, or malformed observation permanently retires
installation-scoped routes, links, ownership, workers, and report-submission authority
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
fact references from canonically confirmed incoming reports or successful
`slack_send` results.
Targets, same-agent reaction ownership, in-flight reservations, and tool-call
attempts are bounded runtime state. Adds establish ownership only after
unambiguous Slack success; removes require that ownership. Ambiguous effects are
not adopted.

The focused reactions module owns target authority, ownership, reservations,
attempt replay, capacity/pinning, tool execution, and typed reaction HTTP
outcomes. Its separately injected reaction client keeps that API surface out of
the transport, identity, and message-posting client boundary.
