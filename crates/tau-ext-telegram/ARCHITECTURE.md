# tau-ext-telegram architecture

`std-telegram` is a personal text bridge, not a generic chat abstraction. The
extension process starts to register tools, but it does not contact Telegram
until a Tau agent calls `telegram_register(enabled: true)`.

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
prefix, not by display name. Agent replies sent with `telegram_send` are prefixed
with `[agent_id]` only. Ambiguous plain text receives a Telegram reply and is not
routed.

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
active-reconfigure lock contention, and bot-token/Bot API URL redaction. Live
Telegram checks are manual only and should not be required for normal CI.
