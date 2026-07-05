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
    secrets:
      telegram_bot_token: {}
    config:
      bot_token_secret: telegram_bot_token
      allowed_user_ids: [123456789]
      # Optional for a private chat; if omitted, send /start to link it.
      chat_id: 123456789
      # Optional: override model-visible tool names for multi-bot setups.
      # Tools become <tool_namespace>_register and <tool_namespace>_send.
      # tool_namespace: telegram
```

`allowed_user_ids` is mandatory and must not be empty. `chat_id` is optional for
private chats because `/start` can link one private chat at runtime.
Group/supergroup chats are refused unless their `chat_id` is explicitly
configured.

## Usage

Ask an agent to call this instance's register tool with `enabled: true`
(`telegram_register` for the legacy `std-telegram` instance). The first
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
(`telegram_send` for the legacy `std-telegram` instance). The model cannot choose
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

When running multiple Telegram bot instances in one harness, give each extension
instance a distinct name such as `telegram-work` or set `config.tool_namespace`.
Non-`std-telegram` instances derive tool names from the instance name by
escaping `_` as `__` and `-` as `_d`, for example `telegram-work` publishes
`telegram_dwork_register`, `telegram_dwork_send`, and tool group
`telegram_dwork`.
The built-in `std-telegram` instance keeps the historical `telegram_register`,
`telegram_send`, and `telegram` group names. The namespace is fixed at extension
startup; restart the extension after changing it.

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

The crate also builds a standalone `tau-telegram-gateway` daemon. This is the
first safe slice of the planned single-token multi-session gateway: it owns one
Telegram update stream, takes the same advisory stream lock as the legacy
extension, checks for active webhooks before polling, enforces
`allowed_user_ids`, keeps a durable update offset/recent-update state file, and
opens a private local status socket.

Allowlisted users can use `/start`, `/help`, `/status`, `/sessions`, `/agents`,
`/select-session`, `/select`, `/to`, and `/where`. Session listings use
gateway-local aliases instead of full session ids; selected or explicit routes
queue inbound prompt deliveries for the owning sidecar. Plain text routes only
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
`{"protocol_version":1,"kind":"status"}` request and returns a status snapshot.
It also accepts persistent sidecar requests: `hello`, `heartbeat`,
`register_agent`, `unregister_agent`, and `goodbye`. The gateway treats
registrations as live leases, refreshes them on heartbeat, removes them on
explicit unregister/goodbye or socket disconnect, and prunes them on lease expiry.
`hello`/status responses include a gateway generation and reannouncement hint so
sidecars reconnecting after a gateway restart know to re-send their current
registrations. Heartbeat/register responses can include queued inbound prompt
deliveries for the sidecar to submit to its local Tau harness. Outbound
`telegram_send` through the gateway remains out of scope for this slice.
