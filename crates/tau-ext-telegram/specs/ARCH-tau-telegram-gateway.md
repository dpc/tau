# ARCH-tau-telegram-gateway: Telegram gateway daemon

The gateway shape implements the unconfirmed choice in
[DECISION-tau-ext-telegram-single-token-gateway](DECISION-tau-ext-telegram-single-token-gateway.md).
Its socket, lease, durable-state, loss-window, routing, and resource contracts
are [SPEC-tau-telegram-gateway](SPEC-tau-telegram-gateway.md). Shared stream
ownership is [SPEC-tau-ext-telegram-stream-owner](SPEC-tau-ext-telegram-stream-owner.md).

`tau-telegram-gateway` is the standalone single-process stream owner, status endpoint,
live sidecar registry, and command router for multi-session Telegram. It resolves the
bot token from an environment variable, validates the Bot API base, creates private
state/runtime directories, acquires the shared stream lock, checks `getWebhookInfo`,
loads durable per-stream JSON state, binds a private Unix status socket, and then owns
`getUpdates` polling.

The durable state is scoped by the same non-secret stream fingerprint used for locking.
It stores the next update offset, an optional private-chat link, chat/user-scoped
selected route, recent update ids for restart duplicate suppression, and small counters;
it does not store pending sidecar deliveries. The gateway persists after each update is
intentionally handled or rejected. Successful enqueue into the bounded live sidecar
queue counts as handling, so a gateway exit after offset advancement but before sidecar
drain can lose that queued delivery before fact publication; the queue is not a
durable acknowledgement protocol.
On startup the loaded state is reconciled with the current config: fixed-chat mode
clears private-chat links, and links or selections that no longer match the configured
chat/user allowlist are cleared and persisted before polling starts.

Telegram-visible gateway behavior includes `/start`, `/help`, `/status`, `/sessions`,
`/agents`, `/select-session`, `/select`, `/to`, and `/where`. Plain text routes only
when a selected target or exactly one live registration makes the target unambiguous.
The allowlist is checked before any side effects. Without a fixed `chat_id`, only one
allowlisted private chat can link with `/start`; unconfigured group/supergroup chats are
ignored rather than linked or replied to. The local socket accepts a one-shot versioned
JSON-line `status` request and persistent sidecar `hello`, `heartbeat`,
`register_agent`, `unregister_agent`, `send_message`, and `goodbye` requests up to a
small fixed byte limit. It returns bounded status snapshots, sidecar lease parameters,
and queued inbound message-fact deliveries on sidecar responses; `send_message` returns
bounded operation errors while keeping Telegram destination selection inside the
gateway. Sidecar registrations are live-only leases: they are removed on explicit
unregister, goodbye, socket disconnect, heartbeat expiry, or gateway
restart/reannouncement. Pending deliveries are bounded per sidecar and are dropped if
their route unregisters, transfers ownership, disconnects, or expires before the sidecar
drains them. The socket is private same-UID local IPC, not an authentication boundary;
this MVP bounds request size and closes protocol-error connections but does not attempt
to defend against all same-user local denial-of-service patterns.

The regular `std-telegram`/`tau-ext-telegram` sidecar supports `mode: gateway_client`
for this architecture. In that mode its startup configuration names
`gateway_socket_path` instead of a bot token secret. The sidecar still declares the
existing register/send tools, subscribes to session/agent lifecycle facts, sends `hello`
and persistent `register_agent`/`unregister_agent`/`heartbeat` requests to the gateway,
and emits gateway-delivered inbound text as `message.delivered` to its own
harness. The gateway supplies native update/message, numeric sender, and chat
identity while retaining routing authority. It does not acquire the stream lock,
check webhooks, or call Telegram
`getUpdates`; those remain solely in the gateway daemon. Outbound `telegram_send` is
forwarded over the same socket as `send_message`; the gateway verifies the sidecar-owned
live registration and sends only to its configured or linked active chat, never to a
model-supplied destination.
