# tau-ext-telegram

First-party personal Telegram text bridge for Tau. The built-in extension is
named `std-telegram` and is disabled by default.

## Configuration

Create a Telegram bot with BotFather, store its token as a Tau secret, and enable
the extension:

```yaml
extensions:
  std-telegram:
    enable: true
    # Optional generic per-instance tool prefix. With `work`, tools are
    # work_telegram_register and work_telegram_send.
    # tool_prefix: work
    secrets:
      telegram_bot_token: {}
    config:
      bot_token_secret: telegram_bot_token
      allowed_user_ids: [123456789]
      # Optional for a private chat; if omitted, send /start to link it.
      chat_id: 123456789
```

`allowed_user_ids` is mandatory and must not be empty. `chat_id` is optional for
private chats because `/start` can link one private chat at runtime.
Group/supergroup chats are refused unless their `chat_id` is explicitly
configured.

## Usage

Ask an agent to call this instance's register tool with `enabled: true`
(`telegram_register` when no `tool_prefix` is configured). The first
registration that starts polling checks that Telegram has no active webhook
before claiming success. If a webhook is active, Tau reports a tool error and
does not delete the webhook or drop updates; remove the webhook yourself or
configure a different bot token. Allowed Telegram users can then use:

- `/start` — link a private chat when no `chat_id` is configured and show help;
- `/agents` — list registered Tau agents;
- `/select <agent-id-or-prefix>` — select a target for later plain text;
- `/to <agent-id-or-prefix> <message>` — send one prompt to an agent;
- plain text — route to the selected agent, or to the only registered agent.

Bot-facing command designators are stable `agent_id` values, optionally followed
by `(display name)` for context in listings and selection confirmations. `/select`
and `/to` resolve only a full `agent_id` or an unambiguous `agent_id` prefix, not
display names. Agent replies sent with the send tool are prefixed with
`[agent_id]`.

Agents should reply to Telegram-originated prompts with this instance's send tool
(`telegram_send` when no `tool_prefix` is configured). The model cannot choose
a destination chat; the send tool uses only the configured or linked chat.

The bridge has a single active chat. When `chat_id` is configured, only that
chat can route messages and all replies go there. Without `chat_id`, send
`/start` from one allowlisted private chat before sending any prompt-routing
text or command; other chats cannot replace that link until the extension
restarts or is reconfigured.
When reconfiguration changes or removes the active chat, agents must call
the register tool again before they can send Telegram replies.

The register and send tools are opt-in for each Tau role. Enable the concrete
tool names in the role configuration with `enable_tools` before asking that role
to use the Telegram bridge. Role policy can also target this instance's tool
group, or the shared `telegram:register` and `telegram:send` tool tags.

When running multiple Telegram bot instances in one harness, assign each entry a
distinct generic `tool_prefix`. Instance keys remain operational identity and do
not change tool names. For example, `tool_prefix: work` publishes
`work_telegram_register`, `work_telegram_send`, and group `work_telegram`.
The prefix is fixed at extension startup; restart the extension after changing it.
The removed Telegram-specific `config.tool_namespace` setting is rejected.

## Limitations

The legacy extension MVP is text-only. Attachments are acknowledged as
unsupported. Registrations, selected agents, learned chat link, and Telegram
update offsets are in memory only in legacy local-poll mode. On lazy startup the
extension drains Telegram's existing backlog without routing it; after restart,
Telegram may still redeliver newer updates that were not acknowledged before
shutdown. Telegram webhooks and `getUpdates` polling are mutually exclusive. Only
one local Tau process can poll a given Bot API base plus bot token within the
same Tau state root at a time; another process using the same stream is rejected
with an advisory-lock contention error. Out-of-band consumers can still cause
Telegram HTTP 409 conflicts; Tau surfaces those as warning notices and clears
active registrations rather than silently continuing.

## Gateway daemon MVP

The crate also builds a standalone `tau-telegram-gateway` daemon. This
single-token multi-session gateway owns one
Telegram update stream, takes the same advisory stream lock as the legacy
extension, checks for active webhooks before polling, enforces
`allowed_user_ids`, keeps a durable update offset/recent-update state file, and
opens a private local status socket.

Allowlisted users can use `/start`, `/help`, `/status`, `/sessions`, `/agents`,
`/select-session`, `/select`, `/to`, and `/where`. Session listings use
gateway-local aliases instead of full session ids; selected or explicit routes
queue inbound delivery records for the owning sidecar. Plain text routes only
when the selected target is live or exactly one agent is registered; ambiguous
text gets a Telegram explanation instead of being guessed. Unconfigured
group/supergroup chats are ignored instead of linked or replied to.
Queued inbound deliveries are bounded live gateway state: they are removed when
the sidecar drains them, unregisters, disconnects, or loses the route lease.

Run it with the bot token in an environment variable rather than on the command
line:

```sh
TELEGRAM_BOT_TOKEN=... tau-telegram-gateway \
  --allowed-user-id 123456789
```

Optional flags include `--bot-token-env`, `--chat-id`, `--api-base`,
`--poll-timeout-seconds`, `--state-dir`, and `--runtime-dir`. The private
same-UID local socket accepts a one-shot JSON-line
`{"protocol_version":0,"kind":"status"}` request and returns a status snapshot.
Gateway and sidecar must run the same current socket protocol, which uses
transport-neutral message-fact identity
fields and original message bodies.
It also accepts persistent sidecar requests: `hello`, `heartbeat`,
`register_agent`, `unregister_agent`, `send_message`, and `goodbye`. The gateway
treats registrations as live leases, refreshes them on heartbeat, removes them on
explicit unregister/goodbye or socket disconnect, and prunes them on lease expiry.
`hello`/status responses include a gateway generation and reannouncement hint so
sidecars reconnecting after a gateway restart know to re-send their current
registrations. Heartbeat/register responses can include queued inbound delivery
records for the sidecar to submit locally as transient
`message.delivered_reported`. `send_message`
responses report bounded operation errors without accepting any Telegram
destination from the sidecar.

The normal sidecar can run in no-poll gateway-client mode:

```yaml
mode: gateway_client
gateway_socket_path: /run/user/1000/tau/telegram-gateway/<stream>.sock
```

In this mode the sidecar does not need `bot_token_secret`, does not use
`allowed_user_ids`, and never calls Telegram `getUpdates`; the gateway owns the
token, allowlist, chat policy, polling, and update offset. The sidecar still
publishes the same logical `telegram_register`/`telegram_send` tools (with the
generic `tool_prefix` when configured), tracks the local session and registered agents, registers
live `(session_id, agent_id)` routes with the gateway, and submits queued inbound
deliveries locally as `message.delivered_reported`. Outbound
`telegram_send` goes back through the gateway, which checks that the calling
agent is still registered and sends to the configured or linked active Telegram
chat without accepting a model-supplied destination. A successful local or
gateway send submits `message.sent_reported` before transient
`tool.result_reported`. The harness intercepts committed reports and publishes
the canonical durable facts downstream. Sidecar submission does not acknowledge canonical commit, so
interception, append failure, or a crash may leave transport effects without a
canonical fact.
