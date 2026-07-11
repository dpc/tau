# tau-ext-slack

First-party Slack Socket Mode text bridge for Tau. The built-in extension is
named `std-slack` and is disabled by default.

## Configuration

Create and install a Slack app manually. Store an app-level `xapp-...` token with
`connections:write` and a bot `xoxb-...` token as Tau secrets:

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
      allowed_user_ids: ["U12345678"]
      # Optional: strict (default) or lax. Read the warning below before lax.
      security_mode: strict
      # Optional: mentions_only (default) or all_messages.
      listening_scope: mentions_only
      # Optional. If omitted or empty, one allowlisted DM can link with `start`.
      channel_ids: ["C12345678", "C87654321"]
      # Optional outbound initiation; omission/empty remains reply-only.
      send_destinations:
        - alias: team-ops
          conversation_id: C45678901
          kind: channel
          description: Operations channel
        - alias: incident-thread
          conversation_id: C45678901
          kind: channel
          thread_ts: "1720000000.123456"
          description: Fixed incident thread
        - alias: alice-dm
          conversation_id: D12345678
          kind: dm
          description: Existing direct conversation
      max_message_bytes: 16384
```

`security_mode: strict` forwards only verified humans in `allowed_user_ids`.
`lax` additionally forwards verified human messages, edits, and reactions from
already configured channels (or the already linked DM); non-allowlisted users
cannot link DMs or run bridge commands. Lax substantially expands prompt-injection
exposure but grants no bridge-control or destination-selection authority; accepted ingress activates only its authenticated source-bound reply route.
All Slack payload text remains untrusted in both modes. Identity verification,
allowlist/policy classification, and content trust are separate typed envelope
fields; provider lowering escapes payload text, so lookalike tags have no authority.

`listening_scope: mentions_only` requires an `app_mention` in configured channels; `all_messages` also accepts eligible unmentioned `message` events. This trigger choice is independent of strict/lax sender admission and never admits bots, unconfigured conversations, or bridge control by lax senders. Slack can deliver one mentioned post as both event types; Tau gives both the same `(channel, ts)` durable dedup identity.
In configured channels, bridge commands are recognized only from `app_mention`
events. Unmentioned events accepted by `all_messages` are always prompt content,
even when they begin with `start`, `to`, or `/select`. Linked DMs recognize
commands without a mention.

Every setup needs app-token scope `connections:write`, bot-token scope
`users:read` for ingress identity verification, and `chat:write` for Slack
replies and command responses. Add the rows needed for enabled behavior:

| Behavior | Additional bot event subscriptions | Additional bot token scopes |
| --- | --- | --- |
| Configured-channel mentions | `app_mention` | `app_mentions:read` |
| Channel edits and `all_messages` | `message.channels` | `channels:history` |
| Linked direct messages | `message.im` | `im:history` |
| Reactions to Tau-authored posts | `reaction_added`, `reaction_removed` | `reactions:read` |

Private-channel message/edit support analogously requires `message.groups` and
`groups:history`; MPIM support requires `message.mpim` and `mpim:history`.
Private channels and MPIMs must also be explicitly listed in `channel_ids`;
the empty-`channel_ids` linked-DM mode accepts only a one-to-one IM.
After adding scopes or event subscriptions, reinstall the app to the workspace,
store the refreshed `xoxb-...` token if Slack changed it, and restart Tau.
Invite the app to every configured Slack conversation as Slack requires.
Missing `message.channels`/`channels:history` does not prevent `app_mention`
delivery, but edits cannot arrive because Slack sends them as `message_changed`
under the `message.channels` subscription. The app-level token needs
`connections:write`. Slack App ID, Client ID, Client Secret, and Signing Secret
are not used by this Socket Mode MVP.
Incoming Webhooks, Slash Commands, OAuth redirect URLs, `channels:read`,
`groups:read`, `users:read.email`, `reactions:write`, `chat:write.public`, and
file scopes are also not required. Tau uses configured conversation ids, never
writes reactions, and expects the app to be invited to allowed conversations.

Operator logs distinguish Socket Mode connect/hello, envelope ACK status, and
degraded/reconnecting workers without logging Slack payloads, identifiers, or
secrets. A `users.info` failure logs and emits one bounded warning per
consecutive failure episode; each affected occurrence is rejected and a later
successful verification resets the warning limiter.

## Usage

Ask an agent to call `slack_register` with `enabled: true`. Allowed Slack users
can then use:

- `start` — link a DM when `channel_ids` is empty and show help;
- `agents` — list registered Tau agents;
- `select <agent-id-or-prefix>` — select a target for later plain text;
- `to <agent-id-or-prefix> <message>` — send one prompt to an agent;
- plain text — route to the selected agent, or to the only registered agent.

With `mentions_only`, mention the bot first in channels, for example
`@Tau to agent-abc investigate this`. DMs may omit the mention. Replies from an
agent use `slack_send(message, reply_to)`, where `reply_to` is the opaque
canonical id shown in the typed Tau envelope. There is no channel, user, or
thread argument. Each
configured channel has independent agent selection, and replies return to the
source-bound configured channel (or linked DM) and thread selected by the exact
`reply_to` message id. The model-facing `<tau_message>` advertises `reply="slack_send"` only while that route is live; its `message_id` is passed as `reply_to`. Top-level messages receive top-level replies; thread replies remain in
their originating thread automatically.
For initiation use `slack_send(message, destination)`, for example
`{"message":"report complete","destination":"team-ops"}`.
Allowed users' reaction additions/removals on recent messages posted through
`slack_send` are routed back to the owning agent with channel, thread, message,
reaction, event-kind, and user metadata. Other posts and conversations are
ignored.
Edits of recent committed incoming messages are routed as explicit immutable
edit occurrences to the original agent. Their envelopes reference the canonical
original and Slack revision; unknown or conflicting edits are ignored rather
than treated as new text.

The singular `channel_id` key is intentionally unsupported. Empty, malformed,
or duplicate ids and duplicate user ids are configuration errors.

### Configured proactive transport sends

Slack initiation is separately authorized by the empty-by-default `send_destinations` list. Each record has `alias`, `conversation_id`, `kind` (`channel`, `mpim`, or `dm`), and optional `description` and fixed `thread_ts`. Inbound `channel_ids` and a runtime-linked DM never imply this outbound right. The `slack_send` tool requires `message` plus exactly one of opaque `reply_to` or configured `destination`; the model sees sorted aliases and trusted descriptions, never native Slack IDs or a raw thread selector. Every enabled agent may use every advertised alias without `slack_register`; normal effective role/tool policy is the agent authorization layer.

Aliases must match `^[a-z][a-z0-9_-]{0,63}$`; at most 64 are accepted.
`channel` accepts existing `C`/`G` conversations, `mpim` accepts `G`, and
`dm` accepts an existing `D` conversation—never a `U` user ID. Duplicate aliases
or exact conversation/thread routes, malformed timestamps, blank/control/overlong
descriptions, and unknown fields fail configuration. Different fixed threads in
one conversation are distinct routes.
Descriptions are limited to 120 Unicode scalars; capability metadata is also
bounded at the harness boundary. `mpim` routes use existing `G...` conversations.

The extension and harness both fail closed: they revalidate the authenticated connection, current session generation, live call and actual agent/tool, transport, alias, endpoint, native conversation kind/id, and fixed thread. Successful outgoing facts retain the authorization relation and tool call audit. Same-process retries are bounded; transcript replay never posts remotely, and Tau does not claim exactly-once delivery across crashes or ambiguous Slack responses. Prompt injection can still influence a role already granted `slack_send`; isolate ingress roles or keep their destination set minimal. Slack app membership remains required and `chat:write.public` is not needed.
