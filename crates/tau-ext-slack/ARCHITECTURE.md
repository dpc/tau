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
selected agents per Slack conversation, per-agent authorized reply origins,
learned DM link, duplicate event cache, bot user id, and websocket state are
forgotten when the extension restarts. A bounded post-ownership cache maps
Slack's returned `(channel, ts)` identity for successful `slack_send` posts to
the sending agent and optional thread root. Allowed-user reaction add/remove
events route only through an exact cached identity in an authorized
conversation; arbitrary posts and duplicate retries are ignored. Every id in
`channel_ids` is an allowed
Slack channel with independent agent selection. Unconfigured channels and DMs
cannot route prompts or cause replies. If `channel_ids` is empty, exactly one
allowlisted Slack DM can link itself with `start`; other DMs cannot replace the
learned link until restart or reconfiguration.

Allowed Slack users can use these commands in a DM, or in a configured channel by
mentioning the bot:

- `start` or `/start`
- `agents` or `/agents`
- `select <agent-id-or-prefix>` or `/select <agent-id-or-prefix>`
- `to <agent-id-or-prefix> <message>` or `/to <agent-id-or-prefix> <message>`

Plain text routes when exactly one agent is registered or a selected agent
exists. Command designators always put the stable `agent_id` first, with display
name only as context. Routing a prompt records its authorized originating
conversation for that agent. `slack_send` has no destination argument and fails
until the calling registered agent has such an origin; it sends only to that
configured channel or linked DM. Agent replies are prefixed with `[agent_id]`.

Outbound authorization follows an explicit live-only state machine. Routing an
inbound Slack prompt creates a bounded pending record keyed by an
extension-owned correlation id and containing the exact agent, channel, and
prompt text. A matching live harness-owned `agent.prompt_submitted` durable fact
moves it to accepted state; the matching live `agent.prompt_started` activates
that channel for the agent. Pending plus accepted records share a 1024-entry
limit. Mismatched agent/text/context facts fail closed, and replayed lifecycle
facts are ignored by the live-only subscriptions. An unrelated durable prompt
submission clears the agent's active Slack origin. Busy-agent prompts instead
remain pending through `agent.prompt_queued`; a matching live
`agent.prompt_steered` validates and retires their correlation when folded into
the tool-result follow-up, and always revokes outbound authorization because
that provider prompt mixes the prior turn with newly steered input and has no
single safe reply origin. A context-less follow-up with no intervening submitted
or steered user prompt preserves the existing origin so an agent can investigate
with other tools before replying. A recalled queued prompt retires pending
correlations matching its agent and exact text because recall facts lack the
original correlation id. Unregister, unload, shutdown, and invalid inactive
reconfiguration clear the relevant authorization records.

## Harness boundary

Incoming Slack text is emitted as `extension.prompt_submit_request`. The harness
validates the target loaded agent and owns the resulting durable
`agent.prompt_submitted` fact. This extension must not publish transcript prompt
facts directly.

## Socket Mode

The worker obtains temporary websocket URLs through `apps.connections.open`,
validates that production URLs use `wss` (loopback tests may use `ws`), connects,
acks valid envelopes quickly with `{"envelope_id":"..."}`, and then routes only
text `app_mention` events in configured channels and `message` events in the
linked IM conversation. Slack event ids, or `(channel, ts)`
when no event id is present, are cached in a bounded in-memory duplicate cache so
retries and reconnects do not duplicate prompts.

Socket Mode shutdown is event-driven rather than polling based. The websocket
receive loop races incoming Slack frames against a shared shutdown notification,
and reconnect backoff sleeps race the full backoff delay against the same signal.
This keeps normal reconnect timing unchanged while allowing Tau shutdown to wake
an idle connection or long backoff immediately.
