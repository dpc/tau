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
update checkpoints are in memory only in legacy local-poll mode. Routed updates
advance the poll cursor only after the extension receives their matching
canonical `message.delivered` echo; a missing echo causes Telegram redelivery to
replay the retained report. Non-routed updates advance at processing return and
may repeat best-effort Telegram replies while an earlier routed update blocks
the cursor. `/start` and `/select` repeat as idempotent replacements, but remote
effects are not exactly once. On lazy startup the extension drains Telegram's
existing backlog without routing it; restart forgets in-memory checkpoints and
may therefore drain updates that were not canonically confirmed before
shutdown. Telegram webhooks and `getUpdates` polling are mutually exclusive.
Only one local Tau process can poll a given Bot API base plus bot token within the
same Tau state root at a time; another process using the same stream is rejected
with an advisory-lock contention error. Out-of-band consumers can still cause
Telegram HTTP 409 conflicts; Tau surfaces those as warning notices and clears
active registrations rather than silently continuing.

## Gateway daemon MVP

The crate also builds a standalone `tau-telegram-gateway` daemon that operators
must supervise. This
single-token multi-session gateway owns one
Telegram update stream, takes the same advisory stream lock as the legacy
extension, checks for active webhooks before polling, enforces
`allowed_user_ids`, keeps durable offset/checkpoint state, and
opens a private local status socket.

The daemon returns stable supervisor-facing statuses:

- `0`: clean/help;
- `64` (`EX_USAGE`): malformed CLI or missing/empty token environment value;
- `69` (`EX_UNAVAILABLE`): active webhook, held stream lock, or polling HTTP 409;
- `70` (`EX_SOFTWARE`): unexpected invariant or response-shape failure;
- `74` (`EX_IOERR`): local state, lock, runtime filesystem, or durability failure;
- `75` (`EX_TEMPFAIL`): webhook-preflight transport failure or HTTP
  408/425/429/5xx;
- `78` (`EX_CONFIG`): invalid semantic configuration or permanent API
  authentication/configuration rejection.

Ordinary failures after polling starts still retry internally every five
seconds. Exit status is non-secret control data; bounded stderr diagnostics
remain token-redacted.

Allowlisted users can use `/start`, `/help`, `/status`, `/sessions`, `/agents`,
`/select-session`, `/select`, `/to`, and `/where`. Session listings use
gateway-local aliases instead of full session ids; selected or explicit routes
queue inbound delivery records for the owning sidecar. Plain text routes only
when the selected target is live or exactly one agent is registered; ambiguous
text gets a Telegram explanation instead of being guessed. Unconfigured
group/supergroup chats are ignored instead of linked or replied to.
Routed inbound deliveries are persisted before socket exposure. Each response,
including its newline, is limited to 65,536 bytes. The gateway returns only the
oldest delivery prefix whose actual serialized JSON fits and retains it until
the owning sidecar confirms the exact canonical harness echo. Unregister,
disconnect, lease expiry, and gateway restart suppress delivery without
deleting or retargeting it; re-registration replays the exact report. A record
that cannot fit by itself is rejected without reflecting its content in the
diagnostic. Canonical ACK and contiguous cursor advancement commit together in
the same per-stream state file before the socket returns success. That file
retains the 128 most recent content-free report-ID/route pairs so a reconnecting
sidecar can safely retry an ACK whose successful response was lost. An ACK
carries the report ID and its frozen `(session_id, agent_id)` route; it can
retire that exact record after unregister, disconnect, or lease expiry, but a
mismatched route fails without changing durable state.
Failures before state-file installation roll back cleanly. If parent-directory
sync fails after rename, the gateway marks the outcome commit-unknown, refuses
further state operations, and requires restart instead of claiming rollback.

Run it with the bot token in an environment variable rather than on the command
line:

```sh
TELEGRAM_BOT_TOKEN=... tau-telegram-gateway \
  --allowed-user-id 123456789 \
  --client-secret-file /run/credentials/tau-telegram-gateway/client-secret
```

Optional flags include `--bot-token-env`, `--chat-id`, `--api-base`,
`--poll-timeout-seconds`, `--state-dir`, `--runtime-dir`, and
`--previous-client-secret-file` for key rotation. Client-secret files contain
exactly 64 lowercase hexadecimal characters encoding 32 random bytes. The private
same-UID local socket accepts a one-shot JSON-line
`{"protocol_version":0,"kind":"status"}` request and returns only readiness and
a random gateway generation.
Gateway and sidecar must run the same current socket protocol, which uses
transport-neutral message-fact identity
fields and original message bodies.
The authenticated protocol mutually authenticates `hello`, `challenge`, and `authenticate`
with the shared key before it accepts persistent sidecar requests: `heartbeat`,
`register_agent`, `complete_reannouncement`, `unregister_agent`, `send_message`,
`ack_delivery`, and
`goodbye`. The gateway
treats registrations as live leases, refreshes them on heartbeat, removes them on
explicit unregister/goodbye or socket disconnect, and prunes them on lease expiry.
The authenticated response includes a gateway generation and reannouncement hint
so sidecars reconnecting after a gateway restart know to re-send their current
registrations. Heartbeat/register responses can include queued inbound delivery
records for the sidecar to submit locally as transient
`message.delivered_reported`. `send_message`
responses report bounded operation errors without accepting any Telegram
destination from the sidecar.

The normal sidecar can run in no-poll gateway-client mode:

```yaml
mode: gateway_client
gateway_socket_path: /run/user/1000/tau/telegram-gateway/<stream>.sock
gateway_client_secret: telegram_gateway_client
```

Declare `telegram_gateway_client` in this extension instance's `secrets` list.
Tau delivers that key only through `Configure.secrets`; the sidecar creates a
fresh process-local generation and reuses it across reconnects.

In this mode the sidecar does not need `bot_token_secret`, does not use
`allowed_user_ids`, and never calls Telegram `getUpdates`; the gateway owns the
token, allowlist, chat policy, polling, and update offset. The sidecar still
publishes the same logical `telegram_register`/`telegram_send` tools (with the
generic `tool_prefix` when configured), tracks the local session and registered agents, registers
live `(session_id, agent_id)` routes with the gateway, and submits queued inbound
deliveries locally as `message.delivered_reported`. The sidecar sends
`ack_delivery` only after the configured publisher receives a live canonical
`message.delivered` with the exact report ID, target, and message identity. It
includes that delivery's frozen route, so this final ACK remains valid when the
matching agent has already unloaded.

The sidecar supervises this socket independently of extension configuration
delivery. If the gateway is initially absent or restarts, the sidecar fails
delivery and outbound send closed, reconnects with capped exponential backoff,
sends a fresh `hello`, reannounces the current live route set, and commits it
with `complete_reannouncement` before resuming. Gateway generation changes and
explicit reannouncement hints force
the same replacement path. Reconfiguration and shutdown cancel and join the
previous supervisor, so stale workers cannot restore retired routes.
A same-generation replacement fences every operation on the old connection.
A different live client generation cannot take over an existing route. Socket
authentication removes authority from processes that only inherit socket access,
but it does not contain malicious same-UID `/proc`, ptrace, or memory access.
Outbound
`telegram_send` goes back through the gateway, which checks that the calling
agent is still registered and sends to the configured or linked active Telegram
chat without accepting a model-supplied destination. A successful local or
gateway send submits `message.sent_reported` before transient
`tool.result_reported`. The harness intercepts committed reports and publishes
the canonical durable facts downstream. Missing echoes or uncommitted ACKs
replay the exact retained report. Telegram replies and reports can duplicate
across crashes, so remote effects are not exactly once.
