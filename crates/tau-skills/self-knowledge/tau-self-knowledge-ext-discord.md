---
name: tau-self-knowledge-ext-discord
description: Use when configuring or troubleshooting the separately maintained Tau Discord bridge, including receiver designation, tool authority, routing, or migration.
advertise: false
---

# Tau Discord bridge self-knowledge

`tau-ext-discord` is a separately maintained MessageBridge executable. Its
project owns the exact credential, route, and transport schema. Consult its
`docs/setup.md` for private maintainer guild-channel setup, `SECURITY.md` for
trust boundaries, and `docs/design.md` and `docs/runtime.md` for the full contract
and limits. Do not infer compatibility from the Cargo package version:
the linked-message producer uses registry `dpc-tau-client`/`dpc-tau-proto`
SDK 0.21.0 / protocol 16.6. Update the installed
extension together with a compatible harness before activation, which requires
separate authorization.
Historical SDK 0.7.0 / protocol 9.0 and SDK 0.10.0 / protocol 10.2
binaries are rejected before configuration. Updating the SDK does not establish
full live Discord connectivity.

## Private maintainer channel

Use a normal private guild **text channel**, not strict send-only mode (which
supports a fixed 1:1 DM). Channel permissions and bridge admission are separate:
explicitly allowlist every authorized human with `allowed_user_ids`. Admitted
messages remain untrusted external content.

The owning setup guide supplies a complete names-only harness skeleton. Its
extension-owned `config` uses:

```yaml
bot_token_secret: discord_bot_token
identity_key_secret: discord_identity_key
register_on_start: true
target: canonical_main
message_content: false
allowed_user_ids: [HUMAN_MAINTAINER_ID]
conversations:
  - alias: maintainer_channel
    guild_id: GUILD_ID
    channel_id: PRIVATE_TEXT_CHANNEL_ID
    receive: mentions_only
    proactive_send: true
```

Replace ID placeholders with exact nonzero u64 integers. Omit `send_only`
(defaults false). This admits personal bot mentions in the configured channel,
not ordinary chatter, and does not require privileged guild message-content
intent. Do not broaden it to all messages without explicit operator/provider
approval.

Declare `discord_bot_token: {}` and `discord_identity_key: {}` under the outer
instance's `secrets`. Provision them through Tau's managed-secret sources before
harness startup; see `tau-self-knowledge-secrets`. Never put values in config,
docs, or shell history. The `.yaml` secret files contain raw UTF-8 text, not YAML
mappings. Use a bot token and a separate stable identity key.

Receiver eligibility is not send-tool authorization. For coordinator-only text
tools, deny `discord:*` tags globally, then enable only the final names
`maintainer_discord_register`, `maintainer_discord_conversations`, and
`maintainer_discord_send` on the existing coordinator role when the outer
instance has `tool_prefix: maintainer`. Audit inherited and other explicit
enables, which can restore tools after tag denial. The instance name does not
prefix tools. Do not enable the whole group: it includes reactions. Ordinary
text needs no attachment or Artifact egress grant.

## Startup versus live delivery

A credential-free, network-isolated smoke test of the verified extension reached
Ready on Tau `91d51d60` (wire 10.4) with null and empty config. That proves
protocol admission and lifecycle startup only. Ready or successful registration
does not prove asynchronous Discord authentication/Gateway startup, channel
permissions, receive admission, or actual delivery. Normal sends require current
registration and a live writable transport; use `discord_conversations` (with
the configured prefix) to discover aliases rather than supplying native IDs.

All writes suppress user, role, everyone/here, and reply-notification mentions.
Mention-like text is not a supported notifying ping. Do not automatically retry
failed or uncertain writes: a send may have taken effect without a confirmed
receipt.

## Native mention evidence

Delivered messages and full-body edits may carry optional typed
`mention_evidence`. Absence or a missing boolean means unknown, not false.
The evidence describes at most 32 unique native personal targets using opaque
IDs comparable with sender IDs. Targets can include bots and unadmitted users; role and
everyone/here mentions are excluded. Optional names are untrusted hints bounded
to 256 bytes and 80 Unicode scalars. There is no per-person platform field,
source span, prose guessing, or directory lookup.

Before truncating the list, the producer compares targets with the authenticated
bridge identity, not a particular Tau agent. The people list is partial and cannot
prove absence.
Full and buffered edits replace evidence atomically with their current body.
Frozen typed parent snapshots remain separate; parent mentions do not become
outer-message evidence. Mentions grant no recipient, invitation, admission,
trust, reply selector, or tool capability. Delivery, activation, or an available
reply selector does not mean the message addresses this agent.

## Native reply context

The typed reply-context producer preserves the normalized outer request and may
supply bounded context for an ordinary same-conversation native reply. It quotes
only the matching parent body supplied with that event. Missing content does not
prove deletion. Shared rendering places the request and quote in adjacent
`message` and `quoted_message` text parts, with an available author identity when
useful. The producer never fetches a parent, follows nested replies, or imports
parent attachments. Quoted authors are not authenticated current speakers.

A parent reference is canonical only after an exact existing same-agent,
same-conversation ownership match confirmed on the canonical downpath. Otherwise
`discord-reply-parent:` identifies opaque descriptive data, not a tool capability.
Quoted text and labels remain external data under the shared message renderer;
only the admitted outer message drives commands, mentions, routing, and admission.
The create snapshot stays fixed through delivery, ACKs, retries, and outer edits;
parent changes never refresh it. The producer shrinks or omits optional context
to preserve the unchanged outer body within its limits. Installing this skill
does not deploy or activate the separately maintained producer.
Older producers may still append textual context; the renderer does not parse
those suffixes into typed quotes.

## Linked messages

Enable receive-mode `discord_get_message` (`discord:read`, default off), then use
`{"message_id":"<message_ref or linked ID>"}` to recover current permitted content.
It is outside the core tool group and absent in send-only mode. Its sole argument
is a string of at most 180 characters. Results contain `message_id`, descriptive
`author`, untrusted `text`, and optional `root_id`, `previous_id`, and `reply_to_id`.
Follow only the links you need; there is no search or history-list interface.

`root_id` identifies a proven thread/branch starter, not an ordinary reply parent.
Forum/media child starters can establish it; a public parent also needs independent
receive permission and admission. Private or unproven roots are omitted.
`reply_to_id` separately identifies an ordinary same-channel native reply.
`previous_id` is the immediate predecessor in the same thread or 1:1 DM, proven
by one `before=id&limit=1` probe, never a guild-channel neighbor or an earlier
message substituted for a denied author. Missing links mean unknown.

Live delivery supplies proven forum-root/reply links without new reads; retrieve
the message by its own ID for other available links. Sent facts carry known
root/reply links.
Edits do not inherit links, and reads never refresh frozen quote snapshots.

Native-coordinate `discord-message:v1:` IDs encode guild, channel, message, and
peer as hex (absent guild/peer is zero). They are not secrets or capabilities.
Delivery references add an epoch-hash suffix for ACK uniqueness; `discord_get_message`
accepts that suffix but returns the stable bare locator. These IDs survive restart,
reconnect, and cache eviction with registration and fresh route, participant, and provider
checks. Legacy hashes require retained acknowledged same-agent ownership and
stop resolving after eviction or restart.

Only the current registered receiver can read. Creation-only eligibility or
proactive-DM permission is insufficient. `mentions_only` controls live activation,
not explicit historical reads; Discord's `MESSAGE_CONTENT` rules remain unchanged.
Reads create no delivered facts, activate no agent, and grant no write, reaction,
or attachment authority. Current content may change or disappear; a failed read
does not prove deletion. This path has mock coverage, not live Discord validation.

## Receiver designation

In receive mode, `discord_register {}` means “make the authenticated calling agent
the receiving agent.” It has no arbitrary agent-ID argument. The tool takes empty
arguments; `{"enabled":false}` and direct `discord_unregister` invocations are
rejected rather than creating a pause control. A configured `role` restricts
eligible callers by immutable creation role.
Optional `target: canonical_main` also restricts registration to the harness-owned
main. The default target is ordinary. Canonical targeting requires advertised
protocol 16.5 or newer within 16.x; unsupported revisions fail before Ready or
provider access.

```yaml
register_on_start: true
target: canonical_main
# role is optional; when supplied, it constrains immutable creation role.
```

`register_on_start` defaults to `false`. With this default, the bridge restores a
saved eligible manual designation after restart, but Tau does not select a first
receiver or create one. With `true`, the bridge starts receive processing without
an LLM registration call.
For the ordinary target, it keeps an eligible saved receiver or selects the oldest
eligible existing agent; lazy creation requires admitted external input and an
explicit `role`. Canonical targeting selects only the committed main at startup.
Admitted input may lazily create a missing main without an explicit role or
bootstrap prompt. It never falls back to an ordinary agent or replaces an
unavailable bound main. The target constraint applies to saved restoration,
automatic resolution, manual registration, and legacy migration evidence.
Busy receivers remain sticky, and same-agent registration is a no-op.

The bridge stores a versioned receive designation in session ExtensionData under
`receive-designation.json`; it is not an operator filesystem path. Ephemeral
sessions retain that choice only in memory. The old
`restore_receive_registration` configuration key is rejected for either value with
migration guidance. A missing designation can use one unambiguous historical
registration as migration evidence, but a saved designation wins.

An actual handoff may send a best-effort notice to a still-loaded prior receiver.
Already reported input and its ACK correlation remain with their original agent.
Old source, write, and Artifact authority does not transfer. Separate send,
reaction, attachment, discovery, and send-only grants remain independent of receive
designation.
