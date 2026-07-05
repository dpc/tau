# tau-ext-slack architecture

`std-slack` is a disabled-by-default personal Slack text bridge. It exposes only
`slack_register` and `slack_send`; the model never supplies arbitrary Slack
channel, user, or thread destinations. The extension starts, registers tools,
and waits until an agent calls `slack_register(enabled: true)` before opening
Slack Socket Mode.

Configuration is applied only before the Socket Mode worker starts. Once an
agent registration starts the bridge, the worker owns a cloned `RuntimeConfig`,
including the resolved app and bot tokens, allowlist, routing policy, and message
limit. Later `Configure` messages fail with `ConfigError`; restart Tau to apply
changed Slack credentials, allowlists, routing, or message limits.

## State and routing

Runtime state is intentionally in memory: registered agents, display labels,
selected agents per Slack conversation, learned DM link, duplicate event cache,
bot user id, and websocket state are forgotten when the extension restarts. The
bridge has one active Slack conversation. If `channel_id` is configured, only
that conversation can route prompts and all `slack_send` output goes there. If no
`channel_id` is configured, exactly one allowlisted Slack DM can link itself with
`start`; other DMs cannot replace the learned link until restart or
reconfiguration.

Allowed Slack users can use these commands in a DM, or in a configured channel by
mentioning the bot:

- `start` or `/start`
- `agents` or `/agents`
- `select <agent-id-or-prefix>` or `/select <agent-id-or-prefix>`
- `to <agent-id-or-prefix> <message>` or `/to <agent-id-or-prefix> <message>`

Plain text routes when exactly one agent is registered or a selected agent
exists. Command designators always put the stable `agent_id` first, with display
name only as context. Agent replies sent with `slack_send` are prefixed with
`[agent_id]` only.

## Harness boundary

Incoming Slack text is emitted as `extension.prompt_submit_request`. The harness
validates the target loaded agent and owns the resulting durable
`agent.prompt_submitted` fact. This extension must not publish transcript prompt
facts directly.

## Socket Mode

The worker obtains temporary websocket URLs through `apps.connections.open`,
validates that production URLs use `wss` (loopback tests may use `ws`), connects,
acks valid envelopes quickly with `{"envelope_id":"..."}`, and then routes only
text `app_mention` and IM `message` events. Slack event ids, or `(channel, ts)`
when no event id is present, are cached in a bounded in-memory duplicate cache so
retries and reconnects do not duplicate prompts.

Socket Mode shutdown is event-driven rather than polling based. The websocket
receive loop races incoming Slack frames against a shared shutdown notification,
and reconnect backoff sleeps race the full backoff delay against the same signal.
This keeps normal reconnect timing unchanged while allowing Tau shutdown to wake
an idle connection or long backoff immediately.
