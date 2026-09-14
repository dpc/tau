---
name: tau-self-knowledge-ext-telegram
description: Use for Tau std-telegram setup, routing, tools, gateway-client mode, security, or troubleshooting.
---

# Tau std-telegram extension self-knowledge

`std-telegram` is Tau's disabled-by-default configuration for the separately
maintained `tau-ext-telegram` executable. Tau does not bundle or install that
executable. Install the Tau flake's `tau-ext-telegram` package and ensure it is
available through `PATH` before enabling the instance. Tau still starts it
through the normal supervised stdio extension route. The
[standalone project](https://radicle.network/nodes/radicle.dpc.pw/rad%3Az3sPdSePnxtBvP9pTLwUwVgpMU68r)
owns its source and detailed operational documentation; its Cargo README
installation instructions remain conditional on a future registry publication.
The pinned executable speaks Tau protocol 7.0 using registry SDK 0.4.0.
Tau 7.2 admits this same-major minor skew with a warning.

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

The Tau flake also exports the standalone project's `tau-telegram-gateway`,
which operators must install and supervise separately. Gateway-client mode
configures an exact local gateway socket path and a named per-instance client
secret. The sidecar does not receive the bot token or choose Telegram
destinations in that mode; the gateway owns polling, sender and chat admission,
durable update checkpoints, and outbound routing. Mutual authentication does
not contain malicious same-UID processes, and external Telegram text always
remains untrusted content.

A separate trusted instance can use `mode: gateway_fixed_chat_send` to send
summaries through an existing gateway credential without receiving Telegram
messages. Configure only `gateway_socket_path`, `gateway_client_secret`, and a
nonempty exact `allowed_agent_ids` list; the named secret must already be
declared for that instance, and the gateway must already have its fixed
`--chat-id`. This mode declares only `telegram_send`, never registers or polls,
and never accepts bot-token, destination, Telegram allowlist, or polling fields.
It sends text verbatim without the normal agent prefix and shares the gateway's
existing aggregate rate limit. The shared gateway key still authorizes normal
route operations as well as route-free fixed sends, so this mode is trusted
extension/session policy rather than containment of compromised same-key code.

The standalone project's README, security notes, linked specifications, and
tests own the detailed command, routing, retry, replay, durability, gateway,
exit-status, and troubleshooting contracts.
