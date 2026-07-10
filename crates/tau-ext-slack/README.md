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
      # Optional. If omitted or empty, one allowlisted DM can link with `start`.
      channel_ids: ["C12345678", "C87654321"]
      max_message_bytes: 16384
```

Recommended Slack bot events: `app_mention`, `message.im`, `reaction_added`, and
`reaction_removed`; recommended bot scopes are `chat:write`,
`app_mentions:read`, `im:history`, `reactions:read`, and `users:read`. Slack App ID, Client ID,
Client Secret, and Signing Secret are not used by this Socket Mode
MVP.

## Usage

Ask an agent to call `slack_register` with `enabled: true`. Allowed Slack users
can then use:

- `start` — link a DM when `channel_ids` is empty and show help;
- `agents` — list registered Tau agents;
- `select <agent-id-or-prefix>` — select a target for later plain text;
- `to <agent-id-or-prefix> <message>` — send one prompt to an agent;
- plain text — route to the selected agent, or to the only registered agent.

In channels, mention the bot first, for example
`@Tau to agent-abc investigate this`. DMs may omit the mention. Replies from an
agent use `slack_send`; there is no channel, user, or thread argument. Each
configured channel has independent agent selection, and replies return to the
configured channel (or linked DM) that most recently routed that agent a prompt.
Allowed users' reaction additions/removals on recent messages posted through
`slack_send` are routed back to the owning agent with channel, thread, message,
reaction, event-kind, and user metadata. Other posts and conversations are
ignored.

The singular `channel_id` key is intentionally unsupported. Empty, malformed,
or duplicate ids and duplicate user ids are configuration errors.
