# tau-ext-slack

First-party, disabled-by-default Slack Socket Mode text bridge (`std-slack`). It
exposes `slack_register`, `slack_conversations`, `slack_send`, and the separately
authorized `slack_react`; a per-instance `tool_prefix` scopes all four tools and their group for multi-account deployments.

## Configuration

Create a Slack app, install it to the workspace, invite it to every configured
conversation, and store its `xapp-...` and `xoxb-...` tokens as Tau secrets.

```yaml
extensions:
  std-slack:
    enable: true
    secrets:
      slack_app_token: {}
      slack_bot_token: {}
    config:
      app_token_secret: slack_app_token
      bot_token_secret: slack_bot_token
      allowed_user_ids: ["U12345678", "W23456789"]
      security_mode: strict
      prefix_agent_id: false
      conversations:
        - alias: team-ops
          conversation_id: C12345678
          kind: channel
          receive: mentions_only
          proactive_send: true
          description: Operations channel
        - alias: leadership
          conversation_id: G23456789
          kind: channel
          receive: all_messages
        - alias: incident-mpim
          conversation_id: G34567890
          kind: mpim
          receive: all_messages
          proactive_send: true
        - alias: alice-dm
          conversation_id: D45678901
          kind: dm
          receive: all_messages
        - alias: incident-thread
          conversation_id: C12345678
          kind: channel
          thread_ts: "1720000000.123456"
          proactive_send: true
          description: Fixed incident thread
      dynamic_direct_messages:
        receive: all_messages
      max_message_bytes: 16384
```

Each `conversations` record is one exact alias/conversation/kind/thread route.
`receive` independently enables `mentions_only` (`app_mention` events) or
`all_messages` ingress;
`proactive_send: true` independently advertises its alias for initiation. A
record needs at least one permission. `channel` covers public and private native
channels, `mpim` covers group DMs, and `dm` requires an existing `D...`
conversation. DM receive is always `all_messages`. A receive-enabled fixed-thread route matches
replies under its root and the root create itself. A receive-enabled parent
cannot coexist with a receive-enabled child because the parent already includes
all threads; send-only parent/child routes and distinct receive-thread siblings
are valid.

Aliases match `^[a-z][a-z0-9_-]{0,63}$`; `direct-message` is reserved. At most
64 records are accepted. IDs and timestamps are exact and unpadded, aliases and
native routes are unique, one native conversation cannot have conflicting
kinds, and unknown fields fail closed. A description is trusted model-visible
operator text, allowed on any active static route, and limited to 120 non-control
Unicode scalars. The removed `channel_ids`, `listening_scope`, and
`send_destinations` keys are hard errors with migration guidance.

Agent replies and proactive sends contain only the supplied `message` text by
default. Set `prefix_agent_id: true` to retain the earlier
`[agent-id] message` presentation. This setting changes presentation only:
authorization, opaque reply selection, destinations, threads, and the
`max_message_bytes` check remain based on the original call. Tau submits one
Slack post and, apart from the optional prefix, does not split or otherwise
rewrite the supplied message.

When upgrading from a release that always added the prefix, add
`prefix_agent_id: true` before restarting if downstream readers or automations
depend on that exact format. Otherwise, no migration is needed and the new
unprefixed default takes effect.

### Migration from removed keys

Choose an alias and correct `kind` for each old `channel_ids` id, and set
`receive` to the old global `listening_scope`. Merge an old proactive destination
with the same exact conversation/thread into that record; keep distinct fixed
threads separate. Replace empty-channel implicit DM mode with
`dynamic_direct_messages`. Convert proactive-only DM/MPIM destinations to static
records without `receive`, then remove all three old keys.

Roles that enabled the whole prefixed `slack` group automatically gain configured
route inventory. Roles granting only exact `slack_send` retain replies and
proactive sends by known current alias; add the matching prefixed
`slack_conversations` tool or `slack:discover` tag when discovery is wanted.
Use separate prefixed instances when roles must not share route inventory.

```yaml
# old
channel_ids: [C12345678]
listening_scope: all_messages
send_destinations:
  - { alias: ops, conversation_id: C12345678, kind: channel }

# new
conversations:
  - alias: ops
    conversation_id: C12345678
    kind: channel
    receive: all_messages
    proactive_send: true
```

Omitting `dynamic_direct_messages` disables dynamic-DM discovery and linking. When enabled, an
allowlisted verified human may send `start` in a one-to-one DM. Tau remembers at
most 64 exact `D id -> U/W user` bindings until restart. Links coexist with
static routes, never become proactive aliases, and grant receive-and-source-reply
authority only. A
static receive-enabled DM route blocks dynamic linkage for that D id; a
proactive-only static DM does not.

### Agent-invoked reactions

`slack_react {message_ref, emoji, action}` is disabled by default and uses the
separate `slack:react` policy tag. It accepts only exact Tau-issued refs from
committed incoming create/edit envelopes or successful `slack_send` results; it
never accepts Slack IDs, timestamps, aliases, Unicode emoji, toggle, list, or
discovery. Add/remove ownership is bounded, runtime-only, same-agent, and
fail-closed across route/config/session changes. Add `reactions:write` and
reinstall the Slack app before enabling it. Whole-group `slack` grants include
this new externally visible mutation surface.

Successful `slack_send` calls now return
`{"status":"sent","message_ref":"slack-msg-v1-..."}` rather than the former
plain `sent Slack message` text. The opaque ref becomes usable only after Tau
accepts durable send completion; the short returned-before-activation window
fails closed, and rejected/missing-ID completions never activate it.

## Slack events and scopes

Every setup needs app-token scope `connections:write` and bot-token scopes
`users:read` and `chat:write`. Add the rows used by the configured policy:

| Surface/behavior | Bot event subscription | Bot token scope |
| --- | --- | --- |
| Public/private channel or MPIM mentions | `app_mention` | `app_mentions:read` |
| Public channel all messages and edits | `message.channels` | `channels:history` |
| Private channel all messages and edits | `message.groups` | `groups:history` |
| MPIM all messages and edits | `message.mpim` | `mpim:history` |
| Static or dynamic one-to-one DM | `message.im` | `im:history` |
| Inbound human reactions on owned posts | `reaction_added`, `reaction_removed` | `reactions:read` |
| Agent-invoked add/remove reactions (optional) | none | `reactions:write` |

Add `reactions:write` only when granting `slack_react`, then reinstall/refresh the app. Reinstall after changing scopes/subscriptions and refresh the bot token if Slack
changes it. The app must be a member of configured conversations.
`chat:write.public`, `channels:read`, `groups:read`, `users:read.email`,
signing secrets, webhooks, slash commands, OAuth redirects,
and file scopes are not required.
One-to-one DMs never use `app_mention`.
Slack `channel_id_changed` makes the exact configured id stale; Tau fails closed
until the operator updates the route and restarts.

## Authorization and routing

`strict` admits only allowlisted Slack-verified live humans. `lax` also admits
verified humans on static routes, but never grants DM linking, agent selection,
bridge commands, or destination control. Dynamic DMs remain exact-user and
allowlist bound even in lax mode. All Slack content remains
`UntrustedExternal`; identity, policy, and control authority are typed
separately.

Outside DMs, commands are recognized only when raw trimmed text begins with the exact
authenticated bot mention, regardless of whether Slack wrapped the occurrence
as `message` or `app_mention`. Later command-looking text is prompt content.
Slack's duplicate wrappers share `(conversation, message timestamp)` durable
identity; local help/control side effects run once. Commands are `start`,
`agents`, `select <agent>`, and `to <agent> <message>` (with optional `/`).
Selection is per configured receive route: parent-route threads share selection,
receive-enabled fixed-thread routes are isolated, and dynamic DMs select per D id.

An agent calls `slack_register(enabled: true)` to receive messages. Every
accepted create, edit, or owned-post reaction gets its own opaque, source-bound
reply id. Use `slack_send` with `message` and exactly one of `reply_to` or the
alias-only `destination`; raw Slack ids and thread selectors are never accepted.
Its fixed schema never enumerates configuration. Call `slack_conversations` for
bounded pages (default 20, maximum 32) of all static routes, sorted by alias. Each
record reports only alias, channel/MPIM/DM kind, conversation/fixed-thread scope,
optional operator description, and factual configured receive/proactive policy.
The structured result is
`{"conversations":[{"alias":string,"kind":"channel"|"mpim"|"dm","scope":"conversation"|"fixed_thread","description"?:string,"policy":{"receive":"mentions_only"|"all_messages"|null,"proactive_send":boolean}}],"next_cursor"?:string}`.
`next_cursor`, when present, is passed unchanged as the next request's `cursor`;
cursor input is limited to 128 bytes and each serialized result to 24 KiB.
Discovery is informational and separately
authorizable: it starts no worker, grants no authority, and does not freeze config.
It excludes native ids/roots, dynamic DM links, users/workspaces, registrations,
selections, reply routes, runtime state, and Slack-fetched metadata.
Proactive aliases do not require registration. Threads always use their immutable
root, never a child timestamp, and Tau never sets `reply_broadcast`.

Every role granted this extension instance's `slack_send` can use every current
proactive alias without registration. The extension re-resolves the alias against
current config at send time; no discovery snapshot is required, and same-alias
reuse is operator responsibility. Prompt-injected receive content can
therefore influence proactive sends available to that role. Keep destination
sets and roles minimal; use separate roles or separately prefixed Slack instances
when receive and proactive authority need isolation.

Edits require a recent committed original with matching sender, route, and
thread. Inbound human reaction events require a recent post created through `slack_send`, matching
owner, verified actor, and a covering receive route. Creates survive durable
replay/restart dedup and can restore edit ownership when Slack retries them;
runtime links, selections, reply routes, reaction ownership, and registrations
clear on restart. Tau prevents same-process accepted-send reposts but does not
claim crash-safe exactly-once delivery.

Configuration freezes after successful Socket Mode preflight or immediately
before the first fully authorized Slack post or reaction API attempt. Later configuration is a
restart-required error, including if Slack's post result is ambiguous. Invalid
or denied sends and failed synchronous preflight do not freeze configuration.

Production API/socket endpoints require HTTPS/WSS (plaintext is loopback-test
only). Shutdown and reconnect waits are event-driven. Logs expose bounded,
identifier-free connect/hello/ACK/degraded/reconnect states and redact tokens,
websocket URLs, payloads, envelope ids, and native identifiers. A `users.info`
outage rejects ingress and emits at most one warning per failure episode.
