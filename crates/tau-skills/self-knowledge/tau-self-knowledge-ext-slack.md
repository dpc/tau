---
name: tau-self-knowledge-ext-slack
description: Use this extension skill for Tau std-slack setup, conversation policies, Slack Socket Mode scopes/events, tools, routing, security, edits, reactions, dynamic DMs, or troubleshooting.
advertise: false
---

# Tau std-slack extension self-knowledge

`std-slack` is Tau's disabled-by-default Slack Socket Mode bridge. It exposes
`slack_register`, `slack_conversations`, `slack_send`, and default-off `slack_react`; `tool_prefix` scopes
all four tools and their group for multiple accounts. Slack text is always
untrusted external content.

Configuration requires app/bot token secrets, nonempty exact U/W
`allowed_user_ids`, and an active `conversations` and/or
`dynamic_direct_messages` policy. Each `conversations` item binds a stable alias
to an exact C/G/D conversation, explicit `channel`/`mpim`/`dm` kind, optional
fixed root `thread_ts`, optional `receive: mentions_only|all_messages`, and
independent `proactive_send`, plus an optional operator-authored description on
any active static record. Native ids and raw thread selectors are never model
inputs. `channel_ids`,
`listening_scope`, and `send_destinations` were removed and are hard errors.
Follow the [README migration procedure](../../tau-ext-slack/README.md#migration-from-removed-keys).

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

Agents register to receive. `slack_send` requires `message` plus exactly one
opaque `reply_to` or proactive `destination` alias. Accepted creates, edits, and
owned reactions each receive source-bound reply authority. Edits require a known
committed original; reactions require a recent Tau-authored post and covering
receive policy. Proactive sends need no registration but still require live
harness capability and effective tool policy.

Replies and proactive sends contain only the agent-supplied message by default.
Set `prefix_agent_id: true` to opt into the legacy `[agent-id] message` format.
This presentation setting does not change message limits, post count, opaque
reply authority, routing, threads, authorization, or configuration freeze.

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
sends remain reconfigurable. Runtime links/routes/selections clear on restart;
durable native create dedup survives and Slack retries can restore edit
ownership. Logs and notices omit payloads, ids, websocket URLs, and tokens.

For missing delivery, check the exact `message.*`/`app_mention` subscription and
history/app-mention scope, reinstall after scope changes, verify app membership,
and ensure app/bot tokens share one workspace installation. `missing_scope`
identifies the required scope. `channel_id_changed` makes an exact route stale;
update it and restart. Missing edits/reactions additionally require the broad
message event or both reaction events plus `reactions:read`.

## Agent reactions

Grant `slack_react` or `slack:react` separately (it is disabled by default). It
accepts `{message_ref, emoji, action: add|remove}` only for exact committed
incoming create/edit refs or opaque refs returned by successful `slack_send`.
It accepts no native IDs, aliases, Unicode emoji, list, toggle, or discovery.
Removal is limited to same-agent reactions unambiguously added in the current
runtime. Add `reactions:write`, reinstall the app, and keep the bot a member of
target conversations. Whole Slack-group grants now include this surface.

Successful `slack_send` results use
`{"status":"sent","message_ref":"slack-msg-v1-..."}` (replacing the former
plain-text success). The ref activates only after durable completion acceptance,
so an immediate call can briefly fail closed and rejected completions remain
permanently ineligible.
