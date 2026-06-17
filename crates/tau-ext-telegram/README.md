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
```

`allowed_user_ids` is mandatory and must not be empty. `chat_id` is optional for
private chats because `/start` can link the chat at runtime. Group/supergroup
chats are refused unless their `chat_id` is explicitly configured.

## Usage

Ask an agent to call `telegram_register` with `enabled: true`. Allowed Telegram
users can then use:

- `/start` — link a private chat when no `chat_id` is configured and show help;
- `/agents` — list registered Tau agents;
- `/select <agent-id-or-prefix>` — select a target for later plain text;
- `/to <agent-id-or-prefix> <message>` — send one prompt to an agent;
- plain text — route to the selected agent, or to the only registered agent.

Bot-facing command designators are stable `agent_id` values, optionally followed
by `(display name)` for context in listings and selection confirmations. `/select`
and `/to` resolve only a full `agent_id` or an unambiguous `agent_id` prefix, not
display names. Agent replies sent with `telegram_send` are prefixed with
`[agent_id]`.

Agents should reply to Telegram-originated prompts with `telegram_send`. The
model cannot choose a destination chat; `telegram_send` uses only the configured
or linked chat.

The `telegram_register` and `telegram_send` tools are opt-in for each Tau role.
Enable them in the role configuration with `enable_tools` before asking that role
to use the Telegram bridge. Role policy can also target the `telegram` tool group
or the `telegram:register` and `telegram:send` tool tags.

## Limitations

The MVP is text-only. Attachments are acknowledged as unsupported. Registrations,
selected agents, learned chat id, and Telegram update offsets are in memory only.
On lazy startup the extension drains Telegram's existing backlog without routing
it; after restart, Telegram may still redeliver newer updates that were not
acknowledged before shutdown.
