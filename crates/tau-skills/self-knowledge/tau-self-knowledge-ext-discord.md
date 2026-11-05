---
name: tau-self-knowledge-ext-discord
description: Use when configuring or troubleshooting the separately maintained Tau Discord bridge, including receiver designation, tool authority, routing, or migration.
advertise: false
---

# Tau Discord bridge self-knowledge

`tau-ext-discord` is a separately maintained MessageBridge executable. Its
`docs/setup.md` owns setup, `SECURITY.md` owns trust boundaries, and
`docs/design.md` and `docs/runtime.md` own the full contract and limits.
The linked-message producer uses registry `dpc-tau-client`/`dpc-tau-proto`
SDK 0.21.0 / protocol 16.6. Update the installed extension with a compatible
harness before separately authorized activation. Ready or registration success
does not prove Discord authentication, Gateway connectivity, or delivery.

## Private maintainer channel

Use a private guild **text channel**, not strict send-only mode (a fixed 1:1 DM).
Channel permissions do not replace `allowed_user_ids`; admitted content remains
untrusted. The owning setup guide supplies a complete names-only harness skeleton.
Its extension-owned `config` uses:

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

Replace placeholders with exact nonzero u64 IDs. Omit `send_only` (default false).
This receives personal bot mentions without privileged guild message-content
intent. Broadening reception requires operator/provider approval.

Declare `discord_bot_token: {}` and `discord_identity_key: {}` in the instance's
`secrets`, then provision a bot token and a separate stable identity key through
Tau's managed-secret sources before startup. Secret `.yaml` files contain raw
UTF-8 text, not mappings; see `tau-self-knowledge-secrets`.

Receiver eligibility does not grant tools. For coordinator-only sending with
`tool_prefix: maintainer`, deny `discord:*` tags globally, then enable only
`maintainer_discord_register`, `maintainer_discord_conversations`, and
`maintainer_discord_send` on that role. Audit inherited and explicit enables;
they can restore denied tools. Instance names do not prefix tools, and the whole
group includes reactions.

## Unified sends and original files

Use `discord_conversations` with the configured prefix to discover aliases.
Ordinary receive-mode `discord_send` requires current registration, live writable
transport, and at least one of `reply_to` or configured `destination`; you may
supply both. Every send needs the ordinary send-tool grant and a configured
outbound route: `proactive_send: true` for guild routes, or a fixed
`proactive_direct_messages` alias. Receive-only routes no longer authorize replies.
Operators must review outbound grants; this change does not update them.

`reply_to` takes a returned versioned message locator from delivered/sent facts,
retrieval, linked IDs, copied context or replay. It is inert addressing, not a
grant: no ACK, retained owner, live ingress, same-agent source, target-author
admission or prior read is required. Versioned locators survive restart/eviction;
legacy hashes need surviving native mappings. The tool accepts no raw native route
arguments.

Reply-only shorthand resolves exact configured guild routes or unambiguous fixed
DM aliases. For inherited thread children, supply the parent `destination` plus
the child's `reply_to`; the parent needs outbound permission and `include_threads`.
Exact child policy wins. Creation-only parents grant no arbitrary child access.

A native reply quotes within the selected existing conversation; it never creates
a thread. Explicit `new_thread_name` creates a new named public child under
`allow_thread_creation`, never reuses a duplicate title, and cannot accompany
`reply_to`. `new_thread_tags` applies only to new forum/media children. Existing
join consent, private membership, and active/unlocked checks still apply.
The provider validates the exact reply target; missing targets never fall back to
plain sends. Replies neither fetch target text nor check its author.

Optional `attachments: [{artifact, filename, content_type?}]` accepts at most ten
canonical `<tau-artifact:blake3:…>` references or bare `blake3:` keys, not paths,
URLs or inline bytes. Without attachments, `message` must be nonempty; with
attachments it may be omitted or empty.

Filenames must be 1–128-byte ASCII basenames using letters, digits, `.`, `_`, `-`,
without a leading dot. Optional MIME hints accept image/avif, image/gif,
image/heic, image/jpeg, image/png, image/tiff and image/webp; omission uses
application/octet-stream. Hints do not validate or convert bytes.
The group limit is **16 MiB total**, not per file.

The default-off `discord:send`, exact scoped send grant, and core group authorize
both text and files. Review former text-only roles before upgrading; remove
obsolete `discord_send_attachment` and `discord:attach` grants. There is no
argument-level upload grant. Strict send-only remains fixed-DM/text-only. It requires
`message` and one or both of `destination` and `reply_to`. Replies must resolve the
exact fixed peer. This mode needs no registration or Gateway; attachments (even
`[]`), named creation, reads, reactions
and discovery remain unavailable.

One multipart message contains the group. An incomplete receipt leaves the whole
outcome uncertain; thread creation may leave a child despite send failure.
Do not automatically retry uncertain writes.

Inbound `discord_list_attachments` and `discord_download_attachment` require the
separate `discord:attachments` grant and current same-agent canonical incoming
references. Importing bytes grants no send authority.

## Discover rooms, join threads, and read recent history

These receive-mode tools default off and sit outside the core group. They require
the current receiver and a writable live lifecycle. Prefix tool names for the instance.

- `discord_list_rooms` (`discord:rooms:read`) lists provider metadata, including
  unconfigured rooms. `kind` defaults to `guilds`; `channels` and `active_threads`
  require `guild_id`. Optional `limit` defaults to 50, range 1–50; continue with
  `next_after` as `after`. Native IDs are canonical nonzero decimal u64 strings.
  Inventories are not snapshots. The bridge rejects oversized inventories rather
  than truncating them. It does not enumerate archived threads, DMs, invitations,
  unjoined public guilds, or large-bot inventories requiring a shard selector.
- `discord_join_room {"guild_id":"20","channel_id":"30"}`
  (`discord:rooms:join`) self-joins an existing active, unlocked native thread
  after fresh guild/channel/parent checks. Ordinary channels return unsupported;
  this does not join guilds, accept invitations, create rooms, reopen threads,
  unlock them, or change another user's membership. Do not retry uncertain joins.
- `discord_recent_history {"guild_id":"20","channel_id":"30","limit":10}`
  (`discord:read`) reads configured guild channel/thread receive scope, including
  authorized inherited children under existing exact-child and private-membership
  rules. `limit` defaults to 10, range 1–50. Select a child of forum/media parents;
  DM history is unsupported. Archived/locked threads can be read.
  The bridge fetches one newest provider page and returns self/admitted regular
  or reply messages newest-first, without fetching replacements. `filtered` marks
  excluded candidates, not a complete history scan. Short/empty results prove
  neither silence nor deletion; provider history/content permissions still apply.

Directory names and history text are untrusted. Metadata proves no message access.
Discovery and joining add no local receive route, outbound alias, activation, or
source/attachment authority. History IDs are inert locators readable through
`discord_get_message`, not delivered facts or write grants. Use
`discord_conversations` for configured send aliases. These tools add no configuration
fields or intents; audit existing `discord:read` selectors for the new history tool
and exact declaration/canonical-main veto inventories before enabling anything.

## Room participants

`discord_list_room_participants {"message_id":"…","limit":20}` requires
`discord:participants:read` (default off, outside the core group), the current
receiver, and a receive-authorized room locator. It is absent in send-only mode.

- DMs return `basis: "direct_recipients"`, including self.
- Threads return `basis: "thread_members"` for joined members, not viewers.
  They require the application's `GUILD_MEMBERS` entitlement.
- Ordinary guild channels return `status: "unsupported"` and `complete: false`;
  group DMs remain unsupported.

`limit` defaults to 20, range 1–100. Pass `next_after` as `after`; it is an
exclusive nonzero decimal u64 user-ID cursor. A full thread page needs continuation,
possibly to an empty terminal page. `complete` marks traversal end, not an atomic
roster. Rows include native `user_id`, `is_self`, and optional untrusted
`display_name`. Membership grants neither presence information nor tool authority.

## User profiles and automatic avatars

`discord_get_user_profile {"user_id":"123456789"}` requires
`discord:profiles:read` (default off, outside the core group), current registration
and live transport. It is absent in strict send-only mode.
Use a nonzero decimal native user snowflake, including bot user IDs, not application
IDs. Public lookup may target users outside sender tiers without admitting them.

Opaque `sender.stable_id` works only while this receiver retains an acknowledged,
non-deleted incoming source on an available route. It does not survive restart
or eviction; retain the returned native `user_id` for later lookup.

Results contain untrusted public user metadata, not private OAuth fields, guild
membership, nicknames or roles. Timezone and last-seen are `not_exposed_by_discord`;
presence is `not_collected`, not offline.

The tool imports a static custom PNG requested at 256px or the default avatar,
up to 1 MiB. `avatar.status="imported"` returns an Artifact reference, key, size,
filename and MIME type. Optional avatar failures preserve metadata with
`avatar.status="unavailable"` and `artifact_may_exist`; base lookup failure is an
error. Checks do not prove image safety; cancellation may leave stored artifacts.

## Editing the bot's own profile

`discord_update_self_profile` accepts `username`, `avatar`, and `banner`; omitted
fields stay unchanged. `username` is a non-null 2–32-character string subject to
Discord's name rules. Images use canonical Artifact references or bare `blake3:…`
keys; `null` clears an image. For example,
`{"avatar":"<tau-artifact:blake3:…>","banner":null}` replaces the avatar and clears
the banner.

The separate default-off `discord:profiles:write` grant is outside the core group.
The tool requires the current ordinary receiver and is absent in send-only mode.
Obtain authorization before changing grants or live configuration.
Images must be PNG/JPEG/GIF originals up to 1 MiB each. Empty patches, target IDs,
URLs, paths and inline bytes fail. Discord may reject valid-looking input.
The bridge does not retry or roll back edits; an uncertain result may mean the
profile changed.

## Ordinary send mentions

Existing send permission allows native `<@USER_ID>`, `<@&ROLE_ID>`, `@everyone`
and `@here` in text, captions, DMs, replies and thread/forum starters, including
send-only mode. No separate mention grant is needed. Opaque sender references
are not native mention syntax; separately granted profile/participant reads can
return native IDs.

Discord permissions and notification settings determine effects. A reply alone
does not notify its author (`replied_user: false`); include an explicit mention.
Mentions do not change destination, thread-creation or send authority.

## Native mention evidence

Delivered messages and full-body edits may include `mention_evidence`. Missing
evidence or booleans mean unknown. Up to 32 unique personal targets use opaque IDs
comparable with sender IDs; role and everyone/here mentions are excluded.
Optional names are untrusted. Self comparison refers to the authenticated bridge,
not a Tau agent, and happens before list truncation.

The list can be partial and cannot prove absence. Edits replace evidence with
their body; quoted parents contribute no outer evidence. Delivery, activation,
mentions and reply selectors do not establish that a message addresses this
agent or grant it authority.

## Native reply context

Same-conversation replies may carry a frozen parent quote from the event in
adjacent `message` and `quoted_message` parts. The bridge does not fetch parents,
follow nested replies or import parent attachments. Missing content does not
prove deletion, and quoted authors are not current authenticated speakers.

Parent references may use proven same-conversation native locators without
same-agent canonical ownership. They grant no send, reaction or mutation authority;
unresolvable descriptive references are not capabilities.
Only the outer message controls admission and routing. Parent changes and later
reads do not refresh the snapshot. Older producers may append textual context
instead; the renderer does not parse it into typed quotes.

## Linked messages

Enable `discord_get_message` (`discord:read`, default off, outside the core group)
and call `{"message_id":"<message_ref or linked ID>"}`. The sole argument is at most
180 characters. The tool requires the current ordinary receiver and current
receive authorization; proactive-DM or creation-only permission is insufficient.
`mentions_only` controls live activation, not historical reads; Discord's
`MESSAGE_CONTENT` rules still apply.

Results contain `message_id`, descriptive `author`, untrusted `text`, and available:

- `root_id`: a proven thread/branch starter, not an ordinary reply parent.
- `reply_to_id`: an ordinary same-channel native reply parent.
- `previous_id`: the immediate predecessor in the same thread or 1:1 DM, not a
  guild-channel neighbor or a substitute preceding a denied author.

Follow needed links or use scoped `discord_recent_history`; there is no search tool.
Missing links mean unknown. Live delivery can include proven forum-root/reply links;
read a message by its own ID for other available links.

Native-coordinate `discord-message:v1:` locators survive restart, reconnect and
eviction, subject to fresh policy/provider checks. Delivery references add an
epoch suffix; reads accept it and return the bare locator. Legacy hashes need
retained same-agent ownership and stop resolving after eviction or restart.
Locators are not secrets or capabilities. Reads create no delivered facts or
write/reaction/attachment authority; failed reads do not prove deletion.
This path has mock coverage, not live Discord validation.

## Provider failure diagnostics

On supporting builds, the extension returns and traces bounded REST failure
details: a fixed local stage, observed HTTP status when available and a recognized
numeric Discord code when present. It reports neither status nor code when it
receives no response. It omits unknown or malformed codes, provider messages,
bodies, headers, URLs, tokens, native IDs and exact routes. The extension suppresses
dependency diagnostics; this interface does not expose raw errors.

CDN attachment/avatar failures also retain fixed stages and observed status.
Optional avatar failures preserve public metadata. A failed second step after
thread creation retains its safe cause details but remains an uncertain write;
do not automatically retry or infer rollback from a rejection.

These warnings use the extension's allowlisted tracing target. They do not
provide comprehensive Gateway diagnostics or establish the cause of a past
incident. Missing logs are inconclusive without verified capture and filtering.
This behavior has mock coverage, not live Discord validation.

## Receiver designation

`discord_register {}` selects the authenticated caller, not an arbitrary agent ID.
`{"enabled":false}` and direct `discord_unregister` calls fail; they are not pause
controls. Optional `role` constrains immutable creation role.

```yaml
register_on_start: true
target: canonical_main
# role is optional; when supplied, it constrains immutable creation role.
```

`register_on_start` defaults false: it restores an eligible saved manual choice
but does not choose or create a first receiver. When true, ordinary targeting keeps
an eligible saved receiver or selects the oldest eligible agent; lazy creation
requires admitted input and an explicit `role`.

`target: canonical_main` requires protocol 16.5 or newer within 16.x. It selects
only the committed main; admitted input can create a missing main without a role
or bootstrap prompt. It never substitutes an ordinary agent or replaces an
unavailable bound main. Busy receivers remain selected; same-agent registration
is a no-op.

The designation lives in session ExtensionData as `receive-designation.json`,
not an operator filesystem path; ephemeral sessions retain it only in memory.
The obsolete `restore_receive_registration` key fails for either value.
Without a saved choice, one unambiguous historical registration can supply
migration evidence.

Handoffs do not transfer already reported input, ACK ownership, source/mutation
authority or Artifact authority. Ordinary sends check current
outbound grants and receiver requirements; locators do not grant authority.
Tool grants remain separate from designation.
