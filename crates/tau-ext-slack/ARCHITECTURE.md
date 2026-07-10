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
selected agents per Slack conversation, opaque canonical reply routes,
learned DM link, duplicate event cache, bot user id, and websocket state are
forgotten when the extension restarts. A bounded post-ownership cache maps
Slack's returned `(channel, ts)` identity for successful `slack_send` posts to
the sending agent and optional thread root. Policy-permitted reaction add/remove
events route only through an exact cached identity in an authorized
conversation; arbitrary posts and duplicate retries are ignored. The cached
thread root is the authenticated outbound request conversation, not later Slack
response or reaction metadata. Omitted later thread metadata is tolerated, but
conflicting metadata fails closed. Every id in
`channel_ids` is an allowed
Slack channel with independent agent selection. Unconfigured channels and DMs
cannot route prompts or cause replies. If `channel_ids` is empty, exactly one
allowlisted Slack DM can link itself with `start`; other DMs cannot replace the
learned link until restart or reconfiguration.

Allowlisted Slack users can use these commands in a DM, or in a configured channel by
mentioning the bot:

- `start` or `/start`
- `agents` or `/agents`
- `select <agent-id-or-prefix>` or `/select <agent-id-or-prefix>`
- `to <agent-id-or-prefix> <message>` or `/to <agent-id-or-prefix> <message>`

Plain text routes when exactly one agent is registered or a selected agent
exists. Command designators always put the stable `agent_id` first, with display
name only as context. `slack_send` accepts only envelope text plus the opaque
canonical `reply_to`; it sends to that selector's private configured channel or
linked DM and validated thread. Top-level origins remain top-level. Slack-visible
agent replies are prefixed with `[agent_id]`.

Outbound authorization uses canonical opaque selectors. Each accepted Slack occurrence is submitted through `transport_message_ingress` with structured user, channel/DM, thread, event, message, operation, reaction, identity-assurance, and sender-policy metadata. The harness stamps trust/source/destination and commits exactly one durable `agent.message_incoming` fact. Only its result binds the returned canonical id to the bridge-private Slack route. Identical retries reach the harness durable dedup index; conflicting metadata fails closed. No formatted prefix, duplicate legacy prompt node, exact-text correlation, or prompt lifecycle subscription is used.

`slack_send` requires that canonical id as `reply_to`; it remains a selector rather than a bearer secret. The bridge checks agent ownership and current conversation authorization, posts to the private channel/thread, then reports returned native identity through `complete_transport_send`. The harness independently checks the connection, session generation, agent, tool call, transport, external actor, conversation, and thread before committing the durable outgoing fact and terminal tool result. Unregister, unload, extension/harness disconnect, shutdown, or reconfiguration clears private routes. Socket Mode websocket reconnects preserve them because the authenticated extension connection and harness session are unchanged. Reply routes and in-flight completions are each capped at 1024 entries. Each new harness session clears routes and renews capability registration under a fresh correlation id; stale results from an earlier session are ignored. Typed messages queued while an agent is busy retain their own ids, so steering/coalescing can never choose a route by arrival order.

## Sender admission and trigger scope

Sender admission is independent of event trigger scope. `strict` is the default and admits only allowlisted Slack-verified humans. `lax` additionally admits Slack-verified non-bot humans, but only in configured channels or the already-linked DM. Lax never grants DM linking, agent selection, bridge-command, arbitrary-destination, or route-selection authority; an accepted occurrence does activate the same opaque source-bound reply route for its authenticated actor, conversation, and thread. External content remains `UntrustedExternal`, while typed policy is `Allowlisted` or `LaxPermitted` and identity is independently `VerifiedAccount`. Future mentions-only/all-messages changes may alter trigger scope only; they must not weaken sender, conversation, control, or reply-route checks.

## Harness boundary

Incoming Slack creates and owned-post reactions use the dedicated typed ingress RPC. The harness owns the resulting durable incoming fact, fold, UI projection, and single live wake. Replay is display/model history only and cannot wake an agent or reactivate a route.

Commit results for incoming creates also populate a bounded native
`(channel, ts)` index. `message_changed` is admitted only through that index and
becomes an immutable typed `Edit` targeting the original canonical and native
ids. The editor/original sender, authorized conversation, thread, message
timestamp, and revision must agree; unknown or conflicting edits fail closed.

## Socket Mode

The worker obtains temporary websocket URLs through `apps.connections.open`,
validates that production URLs use `wss` (loopback tests may use `ws`), connects,
acks valid envelopes quickly with `{"envelope_id":"..."}`, and then routes only
text `app_mention` events in configured channels and `message` events in the
linked IM conversation. Slack event ids, or scoped `(channel, ts)` fallback identities, are sent as durable dedup metadata. Bridge-local dedup remains only for command reply side effects; message/reaction retries are resubmitted so harness dedup survives reconnect and restart.

Socket Mode shutdown is event-driven rather than polling based. The websocket
receive loop races incoming Slack frames against a shared shutdown notification,
and reconnect backoff sleeps race the full backoff delay against the same signal.
This keeps normal reconnect timing unchanged while allowing Tau shutdown to wake
an idle connection or long backoff immediately.
