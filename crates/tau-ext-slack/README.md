# tau-ext-slack

First-party, disabled-by-default Slack Socket Mode text bridge (`std-slack`). It
exposes `slack_register`, `slack_conversations`, `slack_send`, and the separately
authorized `slack_react`; a per-instance `tool_prefix` scopes all four tools and their group for multi-account deployments.

Use one configured Slack extension instance for one receiving Tau agent at a
time. Sharing or retargeting one instance across agents is ad hoc best-effort
behavior: do not rely on it for exact cross-agent routing, permanent ownership,
once-only delivery, or cross-agent deduplication. This is an operating
recommendation rather than a runtime-enforced prohibition. Configure separate
extension instances with distinct tool prefixes and route policy when agents need
independent bridges.

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
      sender_aliases:
        - { user_id: U12345678, alias: dpc }
        - { user_id: W23456789, alias: alice }
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

`sender_aliases` optionally binds at most 64 exact U/W accounts one-to-one to
unique aliases using the same grammar. The alias is presentation only: the
native U/W account remains extension-local authority while model context uses an
opaque sender reference, and aliases never grant admission, command, routing,
reply, reaction, or mention authority. Inbound
creates, edits, and reactions take bounded `profile.display_name` from the same
live `users.info` verification call (80 scalars/256 bytes); it is mutable,
untrusted UI presentation and is never the model's primary sender identity.

For incoming exact mentions of the authenticated installation bot, exactly one
eligible leading mention is removed for routing/command compatibility and every
remaining eligible mention is represented in submitted report text as `@slack_bridge`.
The generic fact schema carries no separate mention flag.
Complete equal-length backtick code ranges suppress recognition; escaped,
labeled, partial, case-changed, and literal `@slack_bridge` text do not count.
A successful `slack_register` returns exactly
`{"status":"registered","incoming_transport_reference":"@slack_bridge"}`;
unregister returns `{"status":"unregistered"}`. This is advisory model syntax,
not a bot-id disclosure, capability, routing authority, or egress expansion.
Sending the token posts it literally.

Agent replies and proactive sends contain only the supplied `message` text by
default. Set `prefix_agent_id: true` to retain the earlier
`[agent-id] message` presentation. This setting changes presentation only:
authorization, Tau-issued reply selection, destinations, threads, and the
`max_message_bytes` check remain based on the original call. Tau submits one
initial Slack post attempt and may retry that exact frozen route/body once after
a bounded rate-limit or ambiguous transport/provider outcome. The initial call
and retry wait run outside the serialized protocol reader. This is
process/session at-least-once delivery: if the first outcome was ambiguous, one
or two Slack copies may exist; two ambiguous outcomes can leave zero, one, or
two. Successful retry results include `delivery_copies: one_or_two_possible`.
A live per-channel FIFO holds each logical call through provider I/O and its
possible retry; unrelated channels remain independent. After remote success,
Slack writes transient `message.sent_reported` and then transient
`tool.result_reported` observations through one serialized write-and-flush gate;
the harness later derives canonical facts. Any confirmed writer failure latches
output failure, retires the entire Slack session and all receive/send/reaction
authority, wakes workers, and requests shutdown. Same-`ToolCallId` replay within
the retained session returns its
stable completed result/error without reposting or submitting a duplicate report;
a new call id is new intent.
Writer flush does not acknowledge the downstream harness-authored canonical
fact; interception, append failure, or a crash can leave a Slack effect without
that fact.
There is no durable outbox, `client_msg_id`, restart guarantee, or exactly-once
claim. Apart from the optional prefix, Tau does not split the supplied message.
Agent text containing raw Slack `<@`, `<!`, or `<#` controls is rejected; the
bridge's own reflected help/control/error text is escaped, bounded, and posted
with mrkdwn and link expansion disabled.

For a source-bound reply only, `mention_source_user: true` asks the bridge to
prepend one native mention of the exact verified human already bound to
`reply_to`. The field defaults to false, accepts no user/name/alias argument,
and is invalid with `destination`. Generated mention text is part of the exact
frozen body reused by the sole retry, while the durable logical message remains
the agent-supplied text.

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
locally submitted incoming create/edit reports or successful `slack_send` results.
Refs have the documented opaque `slack-message:<digest>` fact-ID format, but the
tool never accepts channel IDs or timestamps as separate route selectors, aliases,
Unicode emoji, toggle, list, or discovery. Add/remove ownership is bounded, runtime-only, same-agent, and
fail-closed across route/config/session changes. Add `reactions:write` and
reinstall the Slack app before enabling it. Whole-group `slack` grants include
this new externally visible mutation surface.

Successful `slack_send` calls now return
`{"status":"sent","message_ref":"slack-message:<digest>","delivery_copies":"one"|"one_or_two_possible"}`
rather than the former
plain `sent Slack message` text. The fact ref becomes usable only after the
sent-report and result frames are written and flushed locally. A writer failure
does not activate it; this is not a harness commit acknowledgement.

## Slack events and scopes

Startup and reconnect require `auth.test` to return both the exact bot U/W id
and installing T workspace id. Each Events API wrapper must then prove that
installation through an exact `context_team_id`, or (when absent) one
unambiguous authorization for the same team and bot when supplied. Missing,
malformed, mixed, ambiguous, or mismatched evidence is ACKed as required but
dropped before identity lookup or any local/ingress effect. Slack Connect actor
home teams may differ; top-level `team_id` alone is not authority.

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
marked `content_trust="external"` in model context; identity, policy, and control
authority are typed separately.

Outside DMs, commands are recognized only when raw trimmed text begins with the exact
authenticated bot mention, regardless of whether Slack wrapped the occurrence
as `message` or `app_mention`. Later command-looking text is prompt content.
Slack's duplicate wrappers share one process-local `(conversation, message
timestamp)` cache key, so recent repeats are dropped. Commands are `start`,
`agents`, `select <agent>`, and `to <agent> <message>` (with optional `/`).
Selection is per configured receive route: parent-route threads share selection,
receive-enabled fixed-thread routes are isolated, and dynamic DMs select per D id.

An agent calls `slack_register(enabled: true)` to receive messages. Every submitted create, edit, or owned-post reaction report gets its own source-bound
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

Edits require a same-process locally submitted original report with matching sender, route, and
thread. Inbound human reaction events require a recent post created through `slack_send`, matching
owner, verified actor, and a covering receive route. The 4,096-entry received-id
cache is process-local; cached occurrence ids are nonempty, control-free, and at
most 256 bytes. Reactions use the cache only when Slack supplies an event id.
An occurrence is recorded before later identity lookup, local effects, capacity
admission, or local report write, so a transient failure consumes it until eviction
or restart. Eviction, races, or restart may duplicate delivery. All
runtime links, selections, reply routes, reaction ownership, registrations, and
the outbound call ledger clear on restart. The ledger is bounded at 1,024
non-evicting entries through the live session and rejects new calls before
freeze/I/O when full. At most 64 calls own active delivery workers; additional
calls fail before freeze/I/O until capacity returns. Each HTTP response is capped
at 64 KiB and the retry must begin within the 60-second logical-call horizon.
Disconnect/EOF retires authority before protocol cleanup. Already-started
synchronous HTTP is process-owned and may outlive `run` through its 30-second
timeout, but cannot retry or restore local authority after retirement.

Configuration freezes after successful Socket Mode preflight or immediately
before the first fully authorized Slack post or reaction API attempt. Later configuration is a
restart-required error, including if Slack's post result is ambiguous. Invalid
or denied sends and failed synchronous preflight do not freeze configuration.

Production API/socket endpoints require HTTPS/WSS (plaintext is loopback-test
only). The Socket Mode worker sends its first WebSocket Ping after 10 seconds
and repeats every 10 seconds. An independent deadline reconnects 40 seconds
after the latest Pong, so non-Pong traffic and half-open connections cannot
silently preserve stale ingress. Ping, Pong, and ACK writes remain preemptible
by shutdown and that deadline. Runtime timers observe suspend/resume according
to the platform's monotonic-clock behavior; Tau reconnects once the runtime
observes the deadline as expired. The first startup or reconnect failure also
emits one bounded warning for the process lifetime. Shutdown, reconnect, and
send-retry waits are event-driven. Slow
`chat.postMessage`, rate-limit waits, and retry backoff do not block later
protocol tools, session/lifecycle changes, pings, reconnect, or shutdown. Logs
expose bounded, identifier-free connect/hello/ACK/degraded/reconnect states;
Slack HTTP/identity/post failures expose only closed categories, never raw
bodies, error strings, headers, tokens, websocket URLs, payloads, event ids,
native identifiers, or mention text. A `users.info`
outage rejects ingress and emits at most one warning per failure episode.

Supported events reserve a bounded slot before ACK and then enter one persistent
64-occurrence serial admission FIFO. Slow identity checks and local replies do not
block websocket reads, ACKs, Ping/Pong, reconnect, or shutdown. Saturation is
fail-closed: the bridge does not ACK the occurrence and reconnects so Slack can
retry. The handoff is memory-only and does not add a post-ACK durability claim.

`TRACE` diagnostics remain local, bounded, and identifier-free. They may cover
websocket receipt, ACK, admission, identity, post, report submission, and result
timing, but contain no Slack identifiers, text, tokens, URLs, response bodies,
or durable diagnostic events.
