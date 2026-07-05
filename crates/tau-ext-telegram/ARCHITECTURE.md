# tau-ext-telegram architecture

`std-telegram` is a personal text bridge, not a generic chat abstraction. The
extension process starts to register tools, but it does not contact Telegram
until a Tau agent calls this instance's register tool (`telegram_register` for
the legacy `std-telegram` instance) with `enabled: true`.

Tool names are computed from the initial harness configuration before the
extension sends `Ready`. The built-in `std-telegram` instance publishes the
legacy `telegram_register` and `telegram_send` tools in group `telegram`.
Additional instances derive a namespace from their configured instance name by
escaping `_` as `__` and `-` as `_d`, unless `config.tool_namespace` explicitly
sets another ASCII tool namespace. For example, instance `telegram-work`
publishes `telegram_dwork_register` and `telegram_dwork_send` in group
`telegram_dwork`. This keeps same-harness multi-bot setups unambiguous while
preserving existing `std-telegram` role policy. The namespace is immutable until
extension restart because Tau tool declarations are startup declarations.

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
and HTTP 409 contention classification. Legacy local-poll mode and the planned
Telegram gateway owner must use this boundary so accidental same-token reuse
fails closed with the same behavior.

On the idle-to-active transition for the first registered agent, after acquiring
the local lock and before reporting registration success, the extension calls
`getWebhookInfo`. A non-empty webhook URL means Telegram will not serve
`getUpdates`, so registration fails with a user-visible tool error. Tau does not
call `deleteWebhook` or request `drop_pending_updates`; the user must remove the
webhook or choose another bot token. Later local registrations join the already
owned stream and do not re-run this webhook preflight. `getWebhookInfo` cannot
detect another long-poll consumer, so post-activation webhook changes and other
out-of-band consumers are detected reactively through HTTP 409 `getUpdates`
conflicts. When such a conflict is observed, the poller clears active
registrations, releases the stream lock, and emits a warning notice instead of
leaving agents apparently registered.

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
(`telegram_send` for the legacy `std-telegram` instance) are prefixed with
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
`getUpdates` 409 conflict notices, tool namespace derivation/validation, and
bot-token/Bot API URL redaction. Live
Telegram checks are manual only and should not be required for normal CI.
