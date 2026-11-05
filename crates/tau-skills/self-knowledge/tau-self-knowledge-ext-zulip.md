---
name: tau-self-knowledge-ext-zulip
description: Use when configuring, operating, or troubleshooting Tau std-zulip, including event queues, stream/topic and DM routing, tools, security, or mutations.
---

# Tau std-zulip extension self-knowledge

`std-zulip` is disabled-by-default configuration for the separately maintained
`tau-ext-zulip` executable. Install the Tau flake's `tau-ext-zulip` package on
`PATH` before enabling it; Tau supervises it over stdio. The
[standalone project](https://radicle.network/nodes/radicle.dpc.pw/rad%3Az2LFTBWK7VpAwC3Bpxohkh91aqXd)
owns operational, security and routing documentation.

The bridge uses bot email/API-key HTTP Basic authentication, queue registration
and long polling, not webhooks. The linked-message producer uses registry
SDK 0.21.0 / protocol 16.6. Update the installed executable with a compatible
harness before separately authorized activation. Route changes do not fix a
wire-version mismatch.

## Configuration and routing

Configure `site` (HTTPS in production), `bot_email_secret`, `api_key_secret`,
a stable `identity_key_secret`, and nonempty numeric `allowed_user_ids`.
Optional numeric `untrusted_user_ids` must be disjoint from the allowlist.

- Allowlisted content carries `sender_auth=verified_allowlisted`.
- Untrusted-tier content carries `sender_trust=untrusted`, without an authentication
  qualification. The agent prompt determines how to respond to it.
- DM participants and supplied mutation actors must belong to the union of those
  tiers. Sender admission does not authorize proactive DMs.

Keep the identity key stable across API-key rotation; changing it starts a new
opaque sender/conversation/message namespace.

Configure optional sender aliases, `direct_messages: { receive: all_messages }`,
fixed-recipient `proactive_direct_messages` aliases, and name-based stream/topic
routes. Routes independently select `receive: mentions_only|all_messages` and
`proactive_send`. `all_messages` subscribes the bot before queue registration and
does not later unsubscribe it.

Proactive stream names use fixed topics unless a name without `topic` grants
`agent_chosen_topic: true`. Only then may `zulip_send` supply `topic`;
`topic: ""` means general chat. A proactive-DM alias selects its one configured
recipient; callers cannot supply recipient IDs.

For one outbound DM without ingress, set `send_only: true`, omit inbound fields,
and configure exactly one proactive-DM alias. This mode exposes only scoped
`zulip_send`, without a group, and needs no receiver. It sends `message` to that
alias, never polls or activates an agent, and remains text-only. Mode changes
require restart.

## Tools and receive designation

Ordinary tools default off. `tool_prefix` scopes names and the text group;
reaction, profile, participant, message and attachment reads have their own grants.
Reactions require Tau-issued live references. Reply locators are inert addresses
under current outbound grants; reading a message grants no writes.

Use `zulip_conversations` to discover configured destinations for `zulip_send`;
`zulip_react` has a separate reaction tag.

`zulip_register {}` selects the authenticated caller. Legacy
`{"enabled":true}` works; `{"enabled":false}` fails rather than pausing reception.

```yaml
register_on_start: true # register extension automatically
role: coordinator # role to deliver to; optional, constrains registration and automatic delivery
```

`register_on_start` defaults false: it restores an eligible saved manual choice,
but does not choose or create a first receiver. When true, it keeps an eligible
saved receiver or selects the oldest eligible agent. Optional `role` constrains
immutable creation role and is required for ordinary-target lazy creation, which
can follow only admitted input. Unknown roles/unreadable saved choices fail;
unloaded saved agents are not force-loaded.

### Opt-in canonical-main receiver

`target: canonical_main` requires protocol 16.5 or newer within 16.x.
With automatic registration, admitted input can create a missing main without a
role or bootstrap prompt. An optional role still constrains eligibility.
A saved non-main or unavailable main never permits an ordinary substitute.

Receiver targeting does not restrict outbound tools. Harness
`tool_policy.canonical_main_only` takes exact final registered names, not roles,
tags, groups or instance names; normal grants are still required.
Use `<tool_prefix>_<local_tool_name>` for prefixed instances and audit their actual
inventory. Same-role children do not gain main authority.

The extension saves one receiving-agent choice per persistent session/instance;
ephemeral sessions retain it in memory. Same-agent registration is a no-op.
A different eligible caller replaces it after saving; a loaded prior receiver may
get a notice. If no saved choice exists, one unambiguous historical registration
can supply migration evidence.

Handoffs do not transfer reported input, ACK/checkpoint ownership, old source,
attachment or reaction authority. They do not grant outbound tools. Reply
addresses remain inert; ordinary sends check current outbound grants.

### Discover channels, subscribe, and read recent history

These ordinary-mode tools require the current registered receiver and live queue.
They default off and sit outside the text group. Prefix tool names for the instance.

- `zulip_list_channels {"limit":10,"offset":0}` (`zulip:channels:read`)
  lists provider-visible public/subscribed channel names and descriptions,
  including unconfigured channels, sorted by name before paging.
- `zulip_list_topics {"channel":"ops","limit":10,"offset":0}`
  (`zulip:messages:read`) requires configured whole-channel receive scope.
  Topics appear in provider recency order, including canonical empty topics.
  Zulip topics are native threads, not message-parent relations.
- `zulip_get_history {"channel":"ops","topic":"deploy","limit":10}`
  (`zulip:messages:read`) requires configured whole-channel or exact-topic receive
  scope. Omit `topic` only for whole-channel scope; `"topic":""` selects general
  chat, not the whole channel. The bridge returns the newest candidates in
  chronological order, with opaque IDs and external Markdown. It applies current
  author admission without fetching replacements. Read returned IDs with
  `zulip_get_message`.
- `zulip_join_channel {"channel":"public-channel"}` (`zulip:channels:join`)
  checks channel existence, then subscribes only the authenticated bot, including
  outside local configuration. Zulip's name-only API can create a channel if a
  rename/deletion races that check. The user accepted this residual risk; there
  is no intentional creation option or atomic existing-only guarantee. Do not
  automatically retry failed or uncertain subscriptions. Joining does not follow topics.

All limits default to 10, range 1–50. Metadata pages accept `offset` 0–131072
and return a next offset or null; pages are not snapshots. Large provider
inventories can fail the response cap before local paging.
Metadata and message text are untrusted. Discovery, membership, and history reads
grant no receive/send policy, sender admission, activation, delivered facts, source
ownership, attachment, reaction, or mutation authority. These tools are absent in
send-only mode. `zulip_conversations` remains the configured send inventory.
These tools add no configuration fields. Audit broad `zulip:messages:read` policies,
which can match the new topic/history tools, and exact declaration/canonical-main
veto inventories before enabling anything.

### Ordinary send mentions

Send permission allows inline Markdown mentions such as `@**|123**`, including
send-only mode. No separate mention grant is needed. Mentions do not select DM
recipients or grant routes; Zulip policy/settings determine notifications.

Optional `mention_users` accepts 1–32 strings, each at most 128 characters:
canonical nonzero decimal u64 IDs without leading zeroes, or locally resolvable
`zulip-sender:` references from configured tiers, queue self, or retained
participant locators. Decimal targets may be outside sender tiers, including bots.
No directory lookup occurs. Send-only can use decimal IDs without a queue; opaque
references normally cannot resolve there.

The bridge prepends ID-only native tokens in caller order, including duplicates,
separated by spaces, then a space before nonempty authored text/captions.
An empty list fails; omitting it leaves content unchanged. Authored Markdown stays
byte-identical, and existing nonblank-text/attachment requirements still apply.

Sent reports replace only generated prefixes with presentation tokens such as
`@**|zulip-sender:<opaque hash>**`. These are not native mention syntax or a copy
of the wire body. Authored inline mentions and attachment links remain unchanged.
Failed/uncertain sends publish no confirmed sent report.

### Unified sends and original files

`zulip_send` combines text, attachments, mentions and optional reply addressing;
there is no separate reply tool or permission. Both `destination` and `reply_to`
need an ordinary configured outbound route: a stream with `proactive_send: true`
under its exact-topic or `agent_chosen_topic` policy, or one fixed recipient in
`proactive_direct_messages`. Receive-only streams and inbound DMs no longer
authorize replies. Operators must review outbound routes; this change does not
update grants or configuration.

`reply_to` accepts a stable authenticated v2 locator selecting the message's topic
or exact DM address, not a native per-message reply or automatic quote. A cached
address avoids lookup; otherwise one message fetch recovers its current address.
This internal address resolution needs no read grant, receive scope, sender
admission or mention trigger. Cold DM resolution can use the existing bounded
authenticated self-identity query when queue identity is absent; a live receive
queue is not required. Without cached addressing, deleted/inaccessible messages
cannot resolve. Old hash-only IDs and wrong-key locators fail.

Queue replacement, eviction, handoff and restart do not themselves revoke v2
locators or grant sending permission. Fixed-DM routes permit only one exact
configured peer, never group DMs. `reply_to` and caller `topic` are mutually
exclusive. `destination` plus `topic` selects an ordinary Zulip topic without a
separate creation tool. Send-only remains fixed-DM/text-only without `reply_to`.
Ordinary receiver, tool-grant, configuration, shutdown and upload/send lifecycle
checks still apply; reactions, attachment reads and participants retain their
existing live source requirements.

Ordinary `zulip_send` accepts `attachments: [{artifact, filename, content_type?}]`.
The `zulip:send`, exact send and text-group grants authorize both text and uploads.
Review former text-only roles; remove obsolete `zulip_send_attachment` and
`zulip:attach` grants. There is no argument-level upload permission.

As designated receiver, supply at most ten attachments and exactly one
`destination` or `reply_to`. Use canonical Artifact autolinks or bare `blake3:`
keys, not paths, URLs, inline bytes, `key` or `artifact_key` fields. Filenames
must be 1–128-byte ASCII basenames using letters, digits, `.`, `_`, `-`, without a
leading dot. Without attachments, `message` must be nonempty; with attachments
it may be omitted or empty. Substitute a real exported digest and authorized
destination in this example:

```json
{
  "attachments": [{
    "artifact": "<tau-artifact:blake3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef>",
    "filename": "chart.png",
    "content_type": "image/png"
  }],
  "destination": "configured-alias",
  "message": "Generated chart"
}
```

Optional `content_type` accepts exactly `image/avif`, `image/gif`, `image/heic`,
`image/jpeg`, `image/png`, `image/tiff`, or `image/webp`; omission uses
`application/octet-stream`. It declares the multipart type without validating
bytes or inferring from filenames. Zulip controls previews. The Markdown contains
ordinary `[filename](/user_uploads/...)` links.

The limit is **16 MiB per original**, not per group; Zulip may impose a lower
limit. Message-budget reservations can reduce the file count. The bridge uploads
sequentially and sends one final Markdown message with all links.

Uploads and final send are separate effects. Transfer errors report confirmed
upload count and whether another upload may exist; final-send failure may leave
an unknown message outcome. Cancellation can leave remote effects.
Do not claim rollback or retry automatically; inspect the conversation first.
Success returns `status: "sent"`, `message_ref`, `delivery_copies: "one"`.
Send-only rejects nonempty attachment lists.

### Incoming original files

The installed executable must implement `zulip_list_attachments` and
`zulip_download_attachment`. Both require ordinary registration and separate
`zulip:attachments:read`/scoped-tool grants outside the text group.

List using a delivered same-agent `message_ref`; the result contains up to 16
standard Markdown attachments with untrusted names and opaque `attachment_ref`.
Download using the same `message_ref` and one listed `attachment_ref`.
Both tools refetch the current source and enforce receive/source policy; they do
not scan history.

Downloads import exact original bytes, up to 16 MiB, and return name, verified
size and a canonical `blake3:<64 lowercase hex>` key. Use it in `zulip_send` or
core-shell `import`. Byte lookup grants no reply or route authority.
Edits can replace selections; source/access changes, eviction, registration loss
and restart can invalidate them. Relist rather than reusing stale selections.

### Room participants

`zulip_list_room_participants {"message_ref":"zulip-message:…","limit":20,"offset":0}`
requires `zulip:participants:read` (default off, outside the text group), the current
ordinary receiver, and a currently owned inbound source. Historical reads and
proactive sends do not create this authority.

Stream sources need whole-stream receive permission, not just a topic grant.
`basis: "stream_subscribers"` means active subscribed humans/bots, not topic
followers or everyone who can read. DMs use `direct_recipients`, including self.
Membership does not establish presence.

`limit` defaults to 20, range 1–32; `offset` defaults zero, range 0–131072.
Continue with `next_offset` until null. Membership is fetched anew, so rows may
repeat or disappear; `complete` marks page traversal end, not a snapshot.

Rows contain opaque `user_id`, `is_self`, nullable `display_name`/`is_bot`, and
`name_state`. Names are untrusted. Feature level 384+ supports name enrichment;
older/unknown servers return `"unsupported"`, failed/missing names `"unavailable"`.
Oversized rosters can fail even for small pages.
Retained participant IDs can support separately granted profile reads, but
eviction, expiry or restart can invalidate them.

### User profiles and automatic avatars

`zulip_get_user_profile {"user_id":"123"}` requires `zulip:profiles:read` (default
off, outside the ordinary group), the current receiver and queue identity.
It is absent in send-only mode. Canonical nonzero decimal u64 IDs can address
API-visible realm users/bots outside sender tiers. Opaque `zulip-sender:…` works
for configured tiers, self and retained participant locators; unknown opaque IDs
do not trigger directory searches.

Results expose opaque identity and untrusted name, timezone, join date, account
state, bot type, role, supported custom fields and status when available.
Optional failures preserve base data. Email, raw URLs and native-reference fields
are omitted. Bots/inactive accounts have unsupported status/presence.

Presence/timestamps are provider observations, not verified last-seen times;
timestamps may be stale or fall back to join date. Account activation does not
mean online. Missing data means unavailable, not offline.

Eligible avatars import unchanged, up to 2 MiB. `avatar.state="imported"` returns
reference, size and MIME hint; other states include `none`, `blocked_origin`,
`unsupported_url`, `unavailable`, `too_large`, `unsupported_media_type`.
Server-trusted builds accept the exact avatar destination from the configured
server's authenticated selected-profile response. External destinations must use
HTTPS on the default port (443); storage/CDN hosts, custom percent-encoded paths
and signed queries need no per-host configuration. Configured-origin
`/user_avatars/` and `/static/` paths retain their existing restrictions.
The tool accepts neither caller-supplied URLs nor user-message links.

By configuring a server, you trust it to select that one avatar destination, without
granting message, route, sender or activation authority. The bridge maintains no
learned host allowlist and does not filter DNS or private IP addresses: a compromised
server can select a private-address HTTPS destination. The bridge sends avatar
requests without credentials, verifies TLS and refuses redirects. It retains the
URL, time, byte, MIME/signature, concurrency, cancellation, lifecycle and verified
Artifact import limits, and checks storage availability before downloading.
Profile metadata and image bytes remain untrusted. Image checks do
not prove safety. Cancellation can leave stored artifacts.

On server-trusted builds, `avatar.state="blocked_origin"` includes
`failure_source="local_url_policy"` and a fixed `reason`:
`external_scheme_not_https` or `external_nondefault_port`. This reports the
extension's local URL-policy rejection, not a refusal from Zulip. The extension
attempts neither an avatar download nor an Artifact import. An external CDN
hostname alone no longer causes rejection. Older builds could return
`external_host_not_allowed`; that also identified local policy, not a provider
rejection.

Structured warnings use `operation=profile_avatar`, `stage=local_url_policy`,
`category=blocked_origin`, and the same fixed reason. Safe relation fields are
`host_class=configured|gravatar|other|none`,
`configured_scheme=matches|differs`, and
`configured_effective_port=matches|differs`; port comparisons include parsed
defaults. These fields distinguish origin mismatches without exposing hostnames,
URLs or native IDs. Use the returned reason and safe warning fields to diagnose
a rejection; results and traces omit private avatar URL components. Do not request
raw avatar URLs or loosen policy automatically.
An older `blocked_origin` result alone does not identify the specific mismatch
or establish that a later build fixes the reported avatar incident.

### Editing the bot's own avatar

`zulip_update_self_profile` accepts `{"avatar":"<tau-artifact:blake3:…>"}` or a bare
`blake3:…` key. `{"avatar":null}` resets to the realm default, which may not be blank.
The separate default-off `zulip:profiles:write` grant is outside the text group.
It requires the current ordinary receiver and is absent in send-only mode.
Obtain authorization before changing grants or live configuration.

Only the bot's avatar is editable, not names, timezones, statuses or custom fields.
Empty patches, target IDs, URLs, paths and inline bytes fail. Incoming-webhook
credentials cannot use the endpoint; realm policy may reject it.
PNG/JPEG/GIF/WebP originals may be up to 2 MiB; the provider can enforce a smaller
limit or transform them. One upload/reset occurs, without retry or rollback.
Unknown outcomes may mean the avatar changed.

### Linked messages

Enable `zulip_get_message` (`zulip:messages:read`, outside the text group) and call
`{"message_id":"<message_ref or linked ID>"}` as current ordinary receiver.
Reads check current receive routes, author tiers and every DM participant.
Self messages are readable. `mentions_only` limits live activation, not historical
reads.

Results contain `message_id`, descriptive `author`, untrusted `text`,
`content_trust: "external"`, and optional `previous_id`. The latter is the immediate
provider predecessor in the same topic/DM; denied or unproven predecessors are
omitted, not replaced. Zulip supplies no reliable native root/reply evidence, so
`root_id` and `reply_to_id` are absent. Use `zulip_get_history` for scoped recent
channel/topic history; there is no search tool.

Rate-limited delivery omits previous links; read the message by its own ID for
on-demand links. Versioned opaque IDs survive restart/compaction/eviction while
the identity key stays unchanged. Rotation invalidates them; legacy hashes cannot
be recovered.

IDs grant no authority. Reads return current content, create no delivery or
activation, and grant no write, reaction or attachment ownership. Failure does
not prove deletion.

## Live delivery, quotes and catch-up

Zulip Markdown stays exact, including leading bot mentions and editable
quote-and-reply text. Quote attribution is unverified; the bridge does not extract
or fetch a parent. `zulip_send(reply_to)` selects the source topic or exact DM
address under current outbound grants, not an exact native parent.

`mention_evidence` uses the current authoritative `mentioned` flag. A complete
flag list without it means false; missing/malformed flags mean unknown.
Edits replace evidence with their body. Self means the bridge identity, not a
Tau agent; partial people lists cannot prove absence. Evidence, delivery and
reply selectors neither invite an answer nor grant authority.

Creates, edits, deletes, reactions and sends produce generic message reports.
Supplied mutation actors must belong to the full-content sender tiers.
A singular delete without an actor is accepted only against exact current source
ownership; unsupported/ambiguous deletes fail.

`offline_message_catch_up` defaults false. When enabled, the bridge opens a fresh
live queue, retrieves bounded created-message history after its durable checkpoint,
deduplicates overlap, and advances after canonical delivery confirmation.
First use sets a baseline without replay. It does not recover offline edits,
deletes or reactions, nor rescan earlier history after filter changes.
Crash recovery can duplicate messages; live reaction ownership disappears on
restart, but v2 reply locators remain inert addresses under current outbound policy.

## Optional ingress controls

`non_allowlisted_activity: {}` counts otherwise eligible stream activity from
neither full-content tier, discarding bodies. A later normal-tier message in the
same topic may carry one bounded activity note. Untrusted-tier messages neither
add to nor consume it. Counts are best effort, may expire after 24 hours or
restart, and never cause a standalone wake. DMs are excluded.

To limit eligible live ingress:

```yaml
ingress_rate_limit:
  soft_limit: 5
  hard_limit: 20
  window_seconds: 60
  flush_delay_seconds: 30
```

Omission disables it. All four integers are required; unknown fields fail.
Require `1 <= soft_limit <= hard_limit <= 1024`, `window_seconds` 1–86,400,
`flush_delay_seconds` 1–3,600, and at most 256 configured full-content senders.
Send-only rejects this policy.

Each sender's window spans conversations: creates through `soft_limit` deliver
normally, through `hard_limit` queue, and later bodies drop. Unique eligible
creates spend budget even when dropped; invalid/duplicate/unadmitted traffic does
not. Edits/reactions share the actor's hard budget without a soft delay; deletes
do not spend it.

The first queued create sets a fixed flush timer; a below-soft create can also
flush pending work for the same agent. Reports retain separate exact bodies.
The volatile queue holds at most 256 creates/4 MiB globally and 16 creates/256 KiB
per sender. Overflow, re-registration or restart can lose bodies.
Queued sources have no mutation/reaction owner before reporting; edits replace
accepted bodies without extending timers, deletes cancel them, and queued-source
reactions drop.

Hard-dropped stream creates can contribute body-free activity counts to a later
normal-tier same-topic message. Queue-capacity losses do not count.
History catch-up bypasses this live rate policy and still requires canonical ACK
checkpointing; accepting volatile live work is not durable delivery.

## Provider failure diagnostics

On supporting builds, the extension returns safe local operation labels,
observed HTTP status and recognized Zulip error codes for required API failures.
It also emits structured tracing for provider failures. It reports unrecognized
codes as `unknown` and transport failures by operation, without raw error strings
or URLs. Diagnostics omit provider text, secrets, native IDs and exact routes.

Optional enrichment failures preserve useful partial results. Attachment and
avatar failures retain confirmed-upload counts where applicable, remote-outcome
uncertainty and no-retry guidance. Diagnostic details do not prove rollback or
absence of remote effects, or authorize retries or configuration changes.

## Diagnose receive restoration and queue recovery

Zulip writes content-free diagnostics to supervised extension stderr.
The default `zulip=info,warn` filter exposes its info records and global warnings.
Set `TAU_LOG` in the harness-start environment and restart for changes; see
`tau-self-knowledge-debugging-extensions`.

Inspect replay-boundary outcomes, `receiver_resolution`, `restore_worker_spawn`
and `queue_setup`, then queue expiry/replacement and long-poll failures.
Polling diagnostics distinguish body-read, JSON, envelope, queue and events-shape
failures; setup errors retain operation, HTTP status and a bounded Zulip machine
code, not response bodies.

An absent log is inconclusive without verified capture/filtering. Missing
restoration records can reflect absent replay intent rather than a Zulip API
failure. Logs diagnose stages; they do not establish the cause of a past incident.
