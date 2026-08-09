# ARCH-tau-telegram-gateway: Telegram gateway daemon

The gateway socket, lease, durable-state, acknowledgement, routing, and resource contracts
are [SPEC-tau-telegram-gateway](SPEC-tau-telegram-gateway.md). Shared stream
ownership is [SPEC-tau-ext-telegram-stream-owner](SPEC-tau-ext-telegram-stream-owner.md).

`tau-telegram-gateway` is the standalone single-process stream owner, status endpoint,
live sidecar registry, and command router for multi-session Telegram. It resolves the
bot token from an environment variable, validates the Bot API base, creates private
state/runtime directories, acquires the shared stream lock, checks `getWebhookInfo`,
loads durable per-stream JSON state, binds a private Unix status socket, and then owns
`getUpdates` polling.
Startup failures retain typed transport, HTTP-status, local-I/O, and
configuration classes until the process boundary. During polling, local
state/durability failure and HTTP 409 also reach that boundary, while ordinary
Bot API failures keep the existing internal retry. The gateway maps terminating
classes to the stable `sysexits(3)` statuses specified by
[SPEC-tau-telegram-gateway](SPEC-tau-telegram-gateway.md), so a supervisor can
distinguish permanent configuration, temporary preflight, local repair, and
stream ownership without parsing diagnostics.

The durable state is scoped by the same non-secret stream fingerprint used for locking.
It stores the next update offset, an optional private-chat link, chat/user-scoped
selected route, recent update ids, small counters, and ordered mixed update
checkpoints. Routed checkpoints retain the exact sidecar delivery and opaque
report ID; non-routed checkpoints record completed local work. The gateway
persists routing before socket exposure and advances only the contiguous
acknowledged prefix. Socket threads and the polling owner share one transactional
owner for that existing state file. A canonical ACK commits its prefix advancement
there directly and retains one of 128 content-free recent report-ID/route pairs
for idempotent response-loss retries; no second journal participates. A
post-rename durability error poisons this owner and forces restart.
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
`register_agent`, `unregister_agent`, `send_message`, `ack_delivery`, and
`goodbye` requests up to a small fixed byte limit. It returns bounded status snapshots,
sidecar lease parameters,
and pending durable inbound delivery records on sidecar responses;
`ack_delivery` carries the retained report ID and frozen session/agent route to
confirm an exact canonical echo, and `send_message` returns bounded operation
errors while keeping Telegram destination selection inside the gateway. Sidecar
registrations are live-only leases: they are removed on explicit unregister,
goodbye, socket disconnect, heartbeat expiry, or gateway restart/reannouncement.
Lease loss suppresses delivery but does not delete or retarget durable routed
checkpoints; an exact ACK still retires its frozen checkpoint after lease loss.
The socket is private same-UID local IPC, not an authentication boundary;
this MVP bounds request size and closes protocol-error connections but does not attempt
to defend against all same-user local denial-of-service patterns.
Successful sidecar operations expose only the oldest delivery prefix whose exact
serialized JSON line fits the shared 65,536-byte response limit. A record that cannot
fit alone is rejected before persistence with no private content in the diagnostic. The tested
implementation covers maximum-depth batching through both send and heartbeat response
paths, exact boundaries, JSON escaping, and multibyte UTF-8.

The regular `std-telegram`/`tau-ext-telegram` sidecar supports `mode: gateway_client`
for this architecture. In that mode its startup configuration names
`gateway_socket_path` instead of a bot token secret. The sidecar still declares the
existing register/send tools, subscribes to session/agent lifecycle facts, sends `hello`
and persistent `register_agent`/`unregister_agent`/`heartbeat` requests to the gateway,
and emits gateway-delivered inbound text as `message.delivered_reported` to its own
harness. It retains exact correlation before output and sends `ack_delivery`
only after the configured publisher's matching live canonical
`message.delivered` echo, including the delivery's frozen route so the ACK can
succeed after that route retires. The gateway supplies native update/message,
numeric sender, and chat identity while retaining routing authority. It does not
acquire the stream lock, check webhooks, or call Telegram
`getUpdates`; those remain solely in the gateway daemon. Outbound `telegram_send` is
forwarded over the same socket as `send_message`; the gateway verifies the sidecar-owned
live registration and sends only to its configured or linked active chat, never to a
model-supplied destination.

Each gateway-client sidecar has one bounded, cancellable connection supervisor.
An absent socket, disconnect, heartbeat failure, changed gateway generation, or
reannouncement hint retires the current connection and fails socket delivery,
ACK transmission, and send closed. A validated late canonical echo remains
pending locally for ACK over the next validated connection. The supervisor
retries with capped exponential backoff, sends a
fresh `hello`, and reannounces an exact snapshot of current session/agent routes
before making the replacement connection live. Reconfiguration and shutdown
cancel and join the old supervisor; configuration generations prevent stale
workers or responses from mutating replacement state.
