---
name: tau-self-knowledge-ext-slack
description: Use this extension skill for Tau std-slack setup, conversation policies, Slack Socket Mode scopes/events, tools, routing, security, edits, reactions, dynamic DMs, or troubleshooting.
advertise: false
---

# Tau std-slack extension self-knowledge

`std-slack` is Tau's disabled-by-default configuration for the separately
maintained Cargo package `dpc-tau-ext-slack` and its `tau-ext-slack` executable.
Tau does not bundle or install that executable; install it separately and ensure
it is available through `PATH` before enabling the instance. Tau still starts
it through the normal supervised stdio extension route. It exposes
`slack_register`, `slack_conversations`, `slack_send`, and default-off
`slack_react`; `tool_prefix` scopes all four tools and their group for multiple
accounts. Slack text is always untrusted external content.

Configuration requires app/bot token secrets, nonempty exact U/W
`allowed_user_ids`, and an active `conversations` and/or
`dynamic_direct_messages` policy. Each `conversations` item binds a stable alias
to an exact C/G/D conversation, explicit `channel`/`mpim`/`dm` kind, optional
fixed root `thread_ts`, optional `receive: mentions_only|all_messages`, and
independent `proactive_send`, plus an optional operator-authored description on
any active static record. Native ids and raw thread selectors are never model
inputs. `channel_ids`,
`listening_scope`, and `send_destinations` were removed and are hard errors.
Follow the standalone `tau-ext-slack` project's README migration procedure.
Optional `sender_aliases` bind at most 64 exact U/W ids one-to-one to unique
lowercase aliases. They are operator presentation only. The native U/W id stays
authoritative/model-primary; bounded `profile.display_name` from the same
`users.info` call is untrusted UI-only presentation retained per accepted
occurrence.

For inbound exact mentions of the authenticated installation bot, exactly one
eligible leading mention is removed for routing/command compatibility and
remaining eligible mentions become the semantic token `@slack_bridge` in
published text. The generic fact schema carries no separate mention field. Complete
equal-length backtick code ranges suppress recognition. Escaped, labeled,
partial, case-changed, and literal `@slack_bridge` text do not count. Successful
registration returns exactly
`{"status":"registered","incoming_transport_reference":"@slack_bridge"}`;
unregister returns `{"status":"unregistered"}`. The token discloses no native
id, grants no authority or capability, and does not expand egress; outbound
`@slack_bridge` remains literal text.

Dynamic DMs are opt-in, bounded to 64 exact D-to-allowlisted-U/W links,
receive-and-source-reply only, multi-link, and runtime-only. Static receive DM policy wins over
dynamic discovery; proactive-only static DM policy does not block it.

`strict` admits allowlisted Slack-verified humans. `lax` also admits verified
humans on static routes, but not DM linking or control. `mentions_only` means
Slack `app_mention` delivery. Outside DMs, commands
require a leading authenticated bot mention; duplicate `message` and
`app_mention` wrappers normalize identically. Parent routes include all threads;
receive-enabled fixed-thread routes isolate selection and normalize the root create.

Every setup needs `connections:write`, `users:read`, and `chat:write`. Add:

| Surface | Events | Scope |
|---|---|---|
| Mentions in channel/private/MPIM | `app_mention` | `app_mentions:read` |
| Public all/edit | `message.channels` | `channels:history` |
| Private all/edit | `message.groups` | `groups:history` |
| MPIM all/edit | `message.mpim` | `mpim:history` |
| DM | `message.im` | `im:history` |
| Inbound owned-post reactions | `reaction_added`, `reaction_removed` | `reactions:read` |
| Optional agent add/remove reactions | none | `reactions:write` |

Reinstall after scope/event changes and invite the app to every route.
`chat:write.public` is unnecessary. DMs never use `app_mention`.
`auth.test` must return both bot U/W and installing T workspace identity.
Supported wrappers require matching `context_team_id` or one unambiguous
matching authorization; missing/mixed/mismatched evidence is dropped before
identity lookup. Slack Connect actor home team is not installation authority.

Agents register to receive. Accepted Slack creates, edits, deletes, and reactions
submit transient `message.*_reported` events; the harness publishes immutable
canonical `message.*` facts downstream. `slack_send` requires `message` plus
exactly one Tau-issued `reply_to` or proactive `destination` alias. Successfully
published creates and edits install source-bound reply authority only after their
exact canonical facts return to the configured publisher's live downpath. Each
report carries a stable opaque occurrence ID. Deletes revoke authority immediately
and remain pending until the same canonical confirmation. Socket Mode ACK is
separate from Tau commit. Edits require a known canonically confirmed original;
reactions require a recent Tau-authored post and covering
receive policy. Proactive sends need no registration but still require live
extension/session authority and effective tool policy.

Replies and proactive sends contain only the agent-supplied message by default.
Set `prefix_agent_id: true` to opt into the legacy `[agent-id] message` format.
This presentation setting does not change message limits, retry budget, Tau-issued
reply authority, routing, threads, authorization, or configuration freeze.
Agent-authored text may use ordinary mrkdwn but raw `<@`, `<!`, and `<#` Slack
native controls are rejected. Bridge help/control/error output is escaped,
bounded, and sent with mrkdwn/link expansion disabled.
For source replies only, optional `mention_source_user: true` prepends a mention
of the exact verified human already bound to `reply_to`; it defaults false,
accepts no identity argument, is invalid with `destination`, and is frozen into
the exact body reused by the sole retry.

`slack_conversations` is disabled by default and separately authorizable through
its exact prefixed name or `slack:discover` tag. It returns all static routes,
including receive-only records, in bounded sorted pages with alias, kind, scope,
optional description, and factual receive/proactive policy. It is a local
informational read: no worker startup, registration, authority grant, or config
freeze. It excludes native ids/roots, dynamic links, identities, registrations,
selections, runtime state, and Slack-fetched metadata. Group-enabled roles gain
this inventory surface; use exact policy or separate prefixed instances to isolate
it. Send resolves the current alias without a snapshot token, so same-alias reuse
is operator responsibility.

Every role granted one instance's send tool can use all its proactive aliases.
Untrusted receive content can influence such a role, so keep destination sets and
roles minimal; use separate roles or prefixed extension instances for isolation.

Configuration freezes after successful Socket Mode preflight or immediately
before an authorized post or reaction API attempt; restart Tau to change it. Failed preflight and denied
sends remain reconfigurable. A bounded process-local native-id cache replays exact
pending reports and drops repeats after canonical confirmation; cache eviction or
restart may duplicate delivery. Recording precedes identity and report
construction, so an earlier failure remains suppressed until eviction/restart.
Runtime
links/routes/selections and edit ownership clear on restart. Logs and notices
omit payloads, ids, websocket URLs, and tokens.

Socket Mode admission starts fail-closed in every extension process. An
extension restarted after session initialization becomes active only after the
harness supplies its replay-marked current `session.started` snapshot; an
extension present during initialization receives the start live instead.
Catch-up restores only current-session admission. It does not reconstruct
process-local message, reply, edit, reaction, registration, selection, or
deduplication authority.

`slack_send` reserves each accepted `ToolCallId` in a non-evicting 1,024-entry
process/session ledger and moves initial HTTP plus retry waiting off the
serialized protocol reader. It makes one initial attempt and at most one
byte-identical retry after bounded Retry-After or an ambiguous outcome. This is
at-least-once notification delivery: an ambiguous first attempt followed by
success may leave one or two Slack copies; two ambiguous outcomes can leave
zero, one, or two. Successful retry results report
`delivery_copies: one_or_two_possible`. After remote success Slack writes
transient `message.sent_reported` and then transient `tool.result_reported`
observations through one serialized local write-and-flush gate; the harness later
derives canonical facts. The configured publisher's matching canonical
`message.sent` live echo completes the pending ledger and installs local
message/reaction authority; the typed result remains separate.
Same-id/same-argument replay coalesces before that echo, then returns only the
stable result without reposting or republishing; conflicting reuse errors and a
new call id is new intent. Unregister, unload, route/config/session changes, and shutdown cancel
retry and stale publication authority. The ledger clears on
session/process retirement, so there is no durable outbox, `client_msg_id`,
restart guarantee, or exactly-once claim. Provider diagnostics are closed
categories; raw bodies, Slack error text/headers, native ids, mentions, tokens,
and message content never appear in model errors, notices, or logs.
At most 64 calls own active delivery workers, each response is capped at
64 KiB, and retry must begin within the 60-second logical-call horizon.

Supported events reserve one of 64 process-local queued/in-flight admission slots
before ACK. Slow live-human verification and bridge-local replies run on one serial
worker, so they do not block later websocket ACKs, Ping/Pong, reconnect, or
shutdown. Report-bearing work retains its slot through canonical confirmation;
missing echoes saturate admission and reconnect without ACK so Slack can retry.
Socket Mode ACK remains separate from Tau commit. The FIFO survives
reconnect but is memory-only; process death after ACK can still lose an occurrence.
The worker sends its first WebSocket Ping after 10 seconds and repeats every 10
seconds. An independent deadline reconnects 40 seconds after the latest Pong;
other traffic does not refresh liveness, and blocked Ping/Pong/ACK writes remain
preemptible by shutdown and the deadline. This prevents a half-open connection
from silently disabling ingress indefinitely. The first startup or reconnect
failure emits one bounded warning per process. Tungstenite caps each Socket Mode
frame and complete text or binary message at 256 KiB before decoding; equality is
accepted, while the first excess byte drops the socket and uses the same bounded
reconnect path.
For latency troubleshooting, enable `TRACE` and inspect `slack_latency_v1`
monotonic stage markers. They contain bounded classes and process-local ordinals,
not Slack identifiers, message text, tokens, URLs, agent identities, or durable
events; retain them only in bounded local logs.

## Socket Mode reconnection diagnosis

For the next harness start, use:

```sh
TAU_LOG='slack=trace,warn' tau
```

`slack` is the extension's exact tracing target. Set the variable when starting
or restarting the harness; setting it on a later `tau attach` changes only that
new UI process, not the already-running extension.

First find the affected session with `tau session list`, then set its log
directory. The authoritative private stderr file is
`logs/<configured-extension-instance>.log`, so list it rather than assuming the
instance is named `std-slack`:

```sh
logs="${XDG_STATE_HOME:-$HOME/.local/state}/tau/sessions/<session_id>/logs"
find "$logs" -maxdepth 1 -type f -name '*.log' -printf '%f\n'
grep -lE 'target=slack|Slack Socket Mode' "$logs"/*.log
```

The Slack-specific fields are `lifecycle=connected|hello|reconnecting|degraded|stopped`,
`ack=sent|failed`, `failure=socket_worker|installation_restart_required`,
`failure_class=...`, `reconnect_reason=...`, `stop_reason=...`, and
`rejection=...`. The closed `failure_class` and reason fields identify the
worker stage without raw provider or transport detail. The README explains their
connection, heartbeat, and installation meaning. For the standard process, log,
mirror, privacy, and retention workflow, use
`tau-self-knowledge-debugging-extensions`.

These are deliberately closed-category logs: Slack-owned records omit tokens,
websocket URLs, native IDs, message payloads, and provider response text. The
raw per-extension stderr file is nevertheless private and unredacted at its sink
boundary; dependency or custom-extension output can contain identifiers or local
paths. Review it before sharing. See the standalone `tau-ext-slack` project's
README troubleshooting procedure for the detailed reconnection runbook.

For missing delivery, check the exact `message.*`/`app_mention` subscription and
history/app-mention scope, reinstall after scope changes, verify app membership,
and ensure app/bot tokens share one workspace installation. `missing_scope`
identifies the required scope. `channel_id_changed` makes an exact route stale;
update it and restart. Missing edits/reactions additionally require the broad
message event or both reaction events plus `reactions:read`.

## Agent reactions

Grant `slack_react` or `slack:react` separately (it is disabled by default). It
accepts `{message_ref, emoji, action: add|remove}` only for exact locally written
incoming create/edit refs or refs returned by successful `slack_send`. Refs use
the documented `slack:<channel>:<message-ts>` fact-ID form, but it accepts no
channel IDs or timestamps as separate route selectors, aliases, Unicode emoji,
list, toggle, or discovery.
Removal is limited to same-agent reactions unambiguously added in the current
runtime. Add `reactions:write`, reinstall the app, and keep the bot a member of
target conversations. Whole Slack-group grants now include this surface.
Slack success commits local add/remove ownership only after the successful tool
result is written and flushed locally. Writer failure retires the whole Slack
session without retrying or compensating the remote reaction; local flush is
not a harness commit acknowledgement.
The CLI renders inbound reaction action, exact actor U/W plus bounded
display/configured alias, and the exact custom/skin-tone name; it performs no
Unicode emoji lookup.

Successful `slack_send` results use
`{"status":"sent","message_ref":"slack:<channel>:<message-ts>","delivery_copies":"one"|"one_or_two_possible"}`
(replacing the former
plain-text success). The ref activates only after the sent-fact and result frames
are written and flushed locally; this is not a harness commit acknowledgement.
