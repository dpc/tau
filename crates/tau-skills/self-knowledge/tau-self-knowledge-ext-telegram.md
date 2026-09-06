---
name: tau-self-knowledge-ext-telegram
description: Use for Tau std-telegram setup, routing, tools, gateway-client mode, security, or troubleshooting.
---

# Tau std-telegram extension self-knowledge

`std-telegram` is Tau's disabled-by-default configuration for the separately
maintained Cargo package `dpc-tau-ext-telegram` and its `tau-ext-telegram`
executable. Tau does not bundle or install that executable; install it
separately and ensure it is available through `PATH` before enabling the
instance. Tau still starts it through the normal supervised stdio extension
route.

Local-poll mode requires a named bot-token secret, a nonempty numeric
`allowed_user_ids` list, and an optional exact `chat_id`. Without `chat_id`, an
allowlisted user can link one private chat with `/start`; group and supergroup
chats require explicit configuration. Agents become routes only after their
`telegram_register` tool enables registration. The desired registrations remain
in the configured instance's Session-scope state across restarts, but active
routes are restored only for agents still loaded after replay completes.
`telegram_send` can use only the configured or linked chat; the model cannot
choose a native destination.

Assign a distinct generic `tool_prefix` to each configured bot instance.
Tau preserves the `std-telegram` identity, tool role, disabled default,
managed-secret delivery, startup timeout, state and checkpoint roots, publisher
identity, desired-registration restoration, network policy, and supervised
stdio lifecycle when it launches the standalone executable.

The standalone package also provides `tau-telegram-gateway`, which operators
must install and supervise separately. Gateway-client mode configures an exact
local gateway socket path and a named per-instance client secret. The sidecar
does not receive the bot token or choose Telegram destinations in that mode;
the gateway owns polling, sender and chat admission, durable update
checkpoints, and outbound routing. Mutual authentication does not contain
malicious same-UID processes, and external Telegram text always remains
untrusted content.

The standalone project's README, security notes, linked specifications, and
tests own the detailed command, routing, retry, replay, durability, gateway,
exit-status, and troubleshooting contracts.
