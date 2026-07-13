# ARCH-tau-ext-telegram: tau-ext-telegram architecture

`std-telegram` is a personal text bridge, not a generic chat abstraction. The extension process starts to register tools, but it does not contact Telegram until a Tau agent calls this instance's register tool (`telegram_register` without a generic tool prefix) with `enabled: true`.

External ingress is constrained by [ARCH-external-message-boundary](../../../specs/ARCH-external-message-boundary.md).
Structural tool naming follows
[DESIGN-extension-tool-prefixes](../../../specs/DESIGN-extension-tool-prefixes.md).

## State

Runtime state is intentionally in memory: registered agents, labels, selected
agent per chat, learned private chat link, and update offset are forgotten when
the extension restarts. Update offsets and backlog-drain state are scoped to the
Telegram update stream, identified by the Bot API base URL plus bot token. When
that stream identity changes, the extension resets the offset and drains the new
stream before routing messages. The first poll after lazy startup uses
non-long-poll requests to drain Telegram's existing backlog until it receives an
empty batch, so pre-registration messages are not submitted as fresh prompts.
Poll responses captured under an older configuration generation are discarded
instead of advancing offsets, marking the new stream drained, or routing old
updates.

Before the poller issues `getUpdates`, the extension takes a Tau-side advisory
exclusive OS lock for the stream identity under the shared `state/ext` root. The
lock filename and metadata use a BLAKE3 fingerprint over API base plus bot
token, so they identify contention without writing the raw token. A second Tau
process using the same Tau state root and stream fails closed with a clear
registration/configuration error instead of racing Telegram's singleton update
cursor. The poller clones the held lock for each request so unregister,
reconfiguration, or shutdown can stop future polls without dropping the OS lock
while an older `getUpdates` request is still in flight; the in-flight clone is
released only after that request returns.

Stream-owner mechanics live in `src/stream_owner.rs` rather than in the legacy
extension runtime. The module takes a `StreamIdentity` built from Bot API base
URL plus bot token, not the legacy `RuntimeConfig`, and owns the shared advisory
lock, non-secret stream fingerprint, token redaction, webhook-active diagnostic,
and HTTP 409 contention classification. Legacy local-poll mode and the Telegram
gateway owner use this boundary so accidental same-token reuse
fails closed with the same behavior.

## Harness boundary

Incoming Telegram text is emitted as `extension.prompt_submit_request`. The
harness validates the target loaded agent and owns the resulting durable
`agent.prompt_submitted` fact. This extension must not publish transcript prompt
facts directly.

## Routing

Allowed users can use these commands:

- `/agents`
- `/start`
- `/select <agent-id-or-prefix>`
- `/to <agent-id-or-prefix> <message>`

Plain text routes when exactly one agent is registered or a selected agent exists.
Command designators always put the stable `agent_id` first, with display name
only as context in listings and selection confirmations (`agent_id (display
name)`). `/select` and `/to` resolve by full `agent_id` or unambiguous `agent_id`
prefix, not by display name. Agent replies sent with this instance's send tool
(`telegram_send` when no generic `tool_prefix` is configured) are prefixed with
`[agent_id]` only. Ambiguous plain text receives a Telegram reply and is not routed.

The bridge has one active Telegram chat. If `chat_id` is configured, only that
chat can route commands or prompts and outgoing messages always go there. If
`chat_id` is omitted, exactly one allowlisted private chat can link itself with
`/start`; no prompt-routing text or command routes before that link exists, and
other chats cannot replace the link. Applying new config clears stale learned links when a
fixed chat is configured or when the linked user is no longer allowlisted. If
the active chat changes or is removed, registrations and selections are cleared
so agents must explicitly re-register before sending replies into the new chat.

## Testing strategy

Unit tests use a fake Telegram client and in-memory harness channels. They cover
config validation, tool specs/examples, allowlist enforcement, active-chat and
linking privacy invariants, command routing, update offset/backlog behavior,
shutdown lifecycle, advisory update-stream lock acquisition/contention/release,
active-reconfigure lock contention, webhook-active registration refusal,
`getUpdates` 409 conflict notices, generic tool-prefix mapping, and
bot-token/Bot API URL redaction. Live
Telegram checks are manual only and should not be required for normal CI.

Gateway daemon tests use a fake Telegram client plus test-only gateway resources
to cover durable state round-trips/reconciliation, retry-vs-offset advancement
semantics, same-batch redelivery stops, allowlist/group-chat behavior, local
socket parser/response bounds, sidecar heartbeat/lease cleanup, disconnect and
unregister pruning, gateway restart reannouncement hints, command routing,
chat/user-scoped selections, stable alias churn, bounded/stale delivery queues,
socket delivery response shape, and CLI/env parsing. Gateway-client sidecar
tests use fake Unix sockets and in-memory harness channels to cover no-poll
registration, inbound prompt submission, gateway-client outbound send forwarding,
and stale-delivery filtering. Outbound-send tests use fake gateway clients rather
than live Telegram.

## Security and reliability boundary

The bridge is disabled by default and requires an explicit bot-token secret and non-empty user allowlist. It never accepts a model-selected chat id: output is restricted to the configured chat or the one allowlisted private chat linked with `/start`; unconfigured groups and non-allowlisted users are rejected before side effects. Telegram text remains untrusted external input. Reconfiguration fails closed and invalidates old registrations, selections, links, offsets, and in-flight responses.

Long polling is single-owner per API base and bot token, protected by the non-secret stream lock described above. Active webhooks and HTTP 409 conflicts fail visibly rather than deleting remote state or pretending the stream remains owned. Production endpoints require HTTPS; plaintext is loopback-test-only and endpoint overrides reject userinfo, queries, and fragments. Diagnostics never expose bot tokens, token-bearing API URLs, or unexpected private message content.

Gateway mode delegates polling, allowlist, destination, durable offset, and duplicate-suppression authority to [ARCH-tau-telegram-gateway](ARCH-tau-telegram-gateway.md). The sidecar has no bot token, filters deliveries against current local registrations, and clears leases on local or socket lifecycle loss. Its same-UID socket is a trusted local boundary rather than a sandbox, and its bounded live delivery queue is not a durable acknowledgement protocol.
