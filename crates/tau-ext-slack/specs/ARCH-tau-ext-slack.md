# ARCH-tau-ext-slack: tau-ext-slack architecture

External ingress is constrained by [ARCH-external-message-boundary](../../../specs/ARCH-external-message-boundary.md).

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
name only as context. `slack_send` accepts message text plus exactly one opaque
canonical `reply_to` or configured proactive alias; it never accepts a native
channel, user, or thread. Top-level reply origins remain top-level. Slack-visible
agent replies are prefixed with `[agent_id]`.

Outbound authorization uses canonical opaque selectors. Each accepted Slack occurrence is submitted through `transport_message_ingress` with structured user, channel/DM, thread, event, message, operation, reaction, identity-assurance, and sender-policy metadata. The harness stamps trust/source/destination and commits exactly one durable `agent.message_incoming` fact. Only its result binds the returned canonical id to the bridge-private Slack route. Identical retries reach the harness durable dedup index; conflicting metadata fails closed. No formatted prefix, duplicate legacy prompt node, exact-text correlation, or prompt lifecycle subscription is used.

Replies require that canonical id; proactive sends require an advertised alias
bound to an exact configured conversation and optional fixed thread. The bridge
and harness independently validate the connection, session, agent, tool call,
authorization, endpoint, conversation, and thread before committing the durable
outgoing fact and terminal result. Lifecycle changes revoke runtime routes and
capabilities. See [README configuration](../README.md#configured-proactive-transport-sends).

## Sender admission and trigger scope

Sender admission is independent of event trigger scope. `strict` is the default and admits only allowlisted Slack-verified humans. `lax` additionally admits Slack-verified non-bot humans, but only in configured channels or the already-linked DM. Lax never grants DM linking, agent selection, bridge-command, arbitrary-destination, or route-selection authority; an accepted occurrence does activate the same opaque source-bound reply route for its authenticated actor, conversation, and thread. External content remains `UntrustedExternal`, while typed policy is `Allowlisted` or `LaxPermitted` and identity is independently `VerifiedAccount`. The configured `listening_scope` alters trigger scope only and must not weaken sender, conversation, control, or reply-route checks.
Configured-channel bridge commands are recognized only on `app_mention`.
Unmentioned `all_messages` events are prompt content even when command-shaped;
linked DMs continue to recognize commands directly.

Slack `listening_scope` defaults to `mentions_only`; `all_messages` expands only trigger scope in authorized conversations. Verified-human, strict/lax sender policy, bot/self denial, untrusted content, and source-bound reply authorization remain unchanged. Duplicate `message` and `app_mention` delivery of one `(channel, ts)` shares durable dedup identity.

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
acks valid envelopes quickly with `{"envelope_id":"..."}`, and then routes `app_mention` events in configured channels by default; `all_messages` also routes ordinary channel `message` events, while linked DMs use `message`. Canonical `(channel, ts)` identities are sent as durable dedup metadata so overlapping Slack event deliveries compare identically. Bridge-local dedup remains only for command reply side effects; message/reaction retries are resubmitted so harness dedup survives reconnect and restart.

Socket Mode shutdown is event-driven rather than polling based. The websocket
receive loop races incoming Slack frames against a shared shutdown notification,
and reconnect backoff sleeps race the full backoff delay against the same signal.
This keeps normal reconnect timing unchanged while allowing Tau shutdown to wake
an idle connection or long backoff immediately.

Socket Mode observability is a security invariant. Operator logs expose
connected, hello, ACK sent/failed, degraded, and reconnecting milestones plus
static fail-closed rejection categories. They never expose websocket URLs,
tokens, payloads, Slack user ids, or envelope ids; the exact envelope id exists
only in the ACK wire message. Expected policy rejections are debug-level to
avoid warning spam. API and worker degradation is warning-level, bounded and
token-redacted. A users.info outage emits one warning per consecutive failure
episode, rejects each affected occurrence, and rearms only after a successful
users.info response.

## Configured proactive transport sends

Configured aliases provide the only proactive destinations, independently of inbound channels or learned direct-message routes. The extension and harness revalidate the live source-bound route before sending, and replay never posts remotely. The choice and its authorization tradeoffs are recorded in [DESIGN-tau-ext-slack-proactive-sends](DESIGN-tau-ext-slack-proactive-sends.md).

## Security and reliability boundary

The bridge is disabled by default and requires explicit app/bot secrets and a non-empty sender allowlist. Slack text remains untrusted external content even for verified or allowlisted humans; lax admission widens prompt-injection exposure without granting bridge control or arbitrary destination selection. Workspace administrators, Slack, Slack Connect participants, and channel members may be able to read messages, so the bridge makes no end-to-end-encryption claim.

Configuration changes fail closed, clear inactive pre-start state on errors, and require restart after worker startup. Production Web API and Socket Mode endpoints require HTTPS/WSS; plaintext overrides are loopback-test-only and endpoint overrides reject userinfo, queries, and fragments. Logs and model-visible diagnostics are bounded and redact tokens, websocket URLs, envelope ids, private payloads, and other transport credentials.
