---
name: tau-self-knowledge-ext-zulip
description: Use when configuring, operating, or troubleshooting Tau std-zulip, including event queues, stream/topic and DM routing, tools, security, or mutations.
---

# Tau std-zulip extension self-knowledge

`std-zulip` is Tau's disabled-by-default configuration for the separately
maintained `tau-ext-zulip` executable. Tau does not bundle or install that
executable. Install the Tau flake's `tau-ext-zulip` package and ensure the
executable is available through `PATH` before enabling the instance. Tau still
starts it through the normal supervised stdio extension route. The
[standalone project](https://radicle.network/nodes/radicle.dpc.pw/rad%3Az2LFTBWK7VpAwC3Bpxohkh91aqXd)
owns its source and detailed operational documentation. The bridge uses bot
email/API-key HTTP Basic authentication, `POST /api/v1/register`, and long-poll
`GET /api/v1/events`; it does not use webhooks. The historical automatic-receive
revision used Tau protocol 9 and registry SDK 0.7.0, with the receiver-admission
fix first published in Tau commit
`30c8b43410a84294f31f74451f9f7f09a94df473`. That historical requirement is not a
compatibility guarantee for newer harnesses. The linked-message producer uses
registry SDK 0.21.0 / protocol 16.6, including canonical-main receiver support.
Source readiness does not establish the installed bridge's version or activation.
Rebuild it with the compatible harness and configured peers before activation,
which requires separate authorization. Older-protocol binaries are rejected before
configuration or receiver registration; changing Zulip routes or registration
settings does not repair a wire-version mismatch. Zulip remains an original-byte attachment bridge,
not a prepared-image producer or model provider: attachment downloads return
original Artifact keys, and observed typed image references do not grant fetch,
send, or route authority. Linked-message reads require a separate grant; this
does not change existing routes, secret bindings, catch-up checkpoints, or
artifact enumeration authority.

Old readers cannot decode journal records containing the new
`MessagePhase::PartialAnswer` or `ProviderStopReason::Continue` variants.
Neither partial-answer text nor a continuation grants Zulip send, route, or
scheduling authority. Continue is provider/harness turn control, not an
instruction to the bridge.

With a supporting harness, delivered and edited facts carry optional descriptive
`mention_evidence` from Zulip's authoritative current `mentioned` flag.
The people list may be empty or partial; this producer
does not look up directory entries or rewrite native mention tokens. It preserves
raw Markdown byte for byte. For edits, it recomputes evidence from the current
complete list of string flags: `mentioned` means true, a complete list without it
means false, and missing or malformed flags mean unknown. Edits never inherit
create evidence.
Buffered edits replace body and evidence together only when the existing queue
budgets admit both; rejection preserves both.

Mention evidence describes references, not invitations to answer. Self comparison
refers to the authenticated bridge identity, not a particular Tau agent.
Missing evidence or a missing boolean means unknown; a partial people list never
proves absence. Older harnesses may ignore this metadata. Admission, routing,
activation, reply authority, and catch-up handling remain independent of this
evidence; delivery or an available reply selector does not mean the message
addresses this agent.
Mention evidence grants no recipient, invitation, admission, trust, selector, or
tool capability. This producer does not emit typed reply context: editable
quote-and-reply remains exact Markdown, without parent extraction or fetching.

An immediate reply for an already queued event, or a non-blocking poll reply,
may omit the queue-ID echo. The bridge accepts that omission only for its
authenticated request. A supplied echo must still be well-formed and match the
requested queue. Events from either reply follow ordinary admission,
report-before-cursor, and backlog handling; omission never resets the queue or
drops queued messages.

Configure `site`, `bot_email_secret`, `api_key_secret`, a stable
`identity_key_secret`, a nonempty numeric `allowed_user_ids`, and, if wanted, a
numeric `untrusted_user_ids` list disjoint from it. Also configure optional sender
aliases, optional `direct_messages: { receive: all_messages }`, optional
`proactive_direct_messages` aliases with one fixed recipient each, and name-based
stream/topic routes. Eligible content from an `untrusted_user_ids` sender is admitted with
`sender_trust=untrusted` and no sender authentication qualification. Eligible
content from an `allowed_user_ids` sender is admitted with
`sender_auth=verified_allowlisted` and no sender-trust qualification. The two
qualifications are independent: the extension does not synthesize authentication
for the untrusted tier. Direct-message participant admission and supplied mutation
actors use the union of these full-content tiers. The agent prompt owns the policy
for responding to, or taking actions from, untrusted content.

Keep the identity key stable across API-key rotation; changing it deliberately
starts a new opaque sender/conversation/message namespace. Full-content sender
lists admit inbound senders only; they do not authorize proactive DMs. Routes
independently select `receive: mentions_only|all_messages` and `proactive_send`;
every configured channel name resolves to a private native ID before queue
registration, and `all_messages` subscribes the bot idempotently before that
registration without later unsubscribing. Exact proactive stream names remain the
default, while `agent_chosen_topic: true` on a proactive name without `topic`
explicitly grants agent topic choice within that configured channel. Production
requires HTTPS.

Set `non_allowlisted_activity: {}` to collect bounded stream activity that
passes every receive predicate but belongs to neither full-content sender list.
Those third-tier message bodies are discarded. The next normal-tier message in the
same exact stream/topic may prepend one bridge-authored note with sanitized
untrusted display hints, route-scoped opaque pseudonyms, and post counts; its own
Markdown remains the exact suffix, and the pair uses one fact and wake. Untrusted
full-content senders neither add to nor consume this summary. This is best effort,
not a reliable queue: bounded process state can expire or disappear after 24
hours, authority changes, or restart. Capacity can omit new activity,
duplicate-cache eviction can permit duplicate observations, and nothing is
delivered without a later normal-tier message. Direct messages and autonomous
deadline delivery are not supported.

To rate-limit eligible live ingress, configure the optional strict object:

```yaml
ingress_rate_limit:
  soft_limit: 5
  hard_limit: 20
  window_seconds: 60
  flush_delay_seconds: 30
```

Omitting it disables this policy. All four integer fields are required and
unknown fields fail closed: `1 <= soft_limit <= hard_limit <= 1024`,
`window_seconds` is 1–86,400, and `flush_delay_seconds` is 1–3,600. The normal
and untrusted full-content lists together may contain at most 256 configured
senders when the policy is enabled; send-only mode rejects it. Exact native
sender windows cover every eligible conversation for this extension instance,
independently of sender trust. Each unique eligible live create spends its
sender's window, including creates discarded above the hard limit: the first
`soft_limit` creates prompt normally, the next through `hard_limit` enter a
volatile queue, and later bodies are discarded before reports or source
ownership. Invalid, duplicate, self, wrong-route/topic/mention, unregistered,
and rejected-sender traffic spends no budget or flushes the queue.

The first queued create fixes that sender's oldest-pending timer; later arrivals
do not extend it. An independent worker emits separate exact-body reports when
the timer expires. A valid below-soft create can flush pending content for the
same agent across topics: its own bounded sender prefix precedes it, while other
senders are background work. This is not an atomic turn, deadline, or
concatenation guarantee. The queue is deliberately volatile: it holds at most
256 creates or 4 MiB of logical variable data globally, and 16 creates or
256 KiB per sender. A newest candidate that exceeds a cap silently loses its
body and produces no activity summary; queue loss or counter reset can also
follow re-registration, configuration, shutdown, restart, or crash.

A queued base has no reply owner before its report. A valid queued edit replaces
its body without moving the deadline; a rejected hard-limit or cap edit retains
the last accepted body. A delete cancels the queued base and reactions to it
are discarded. Valid live edits and reactions share the actor's hard sender
budget without a soft delay; above-hard activity is dropped without a summary.
Deletes retain their strict actor and frozen-route checks but do not spend rate
budget. This policy separately enables body-free hard-drop activity counts for
otherwise eligible stream creates; `non_allowlisted_activity` independently
counts other rejected senders. Queue-capacity rejection is not hard-drop
activity and creates no summary count. The existing later same-topic
normal/default-trust carrier remains the only summary delivery path, including
normal deferred creates; untrusted below-soft creates can flush pending work but
cannot carry a summary. There is no DM count, standalone summary wake, wrapper,
or trust reclassification.

Volatile acceptance advances only the local catch-up position: it is not
canonical delivery or an ACK and creates no later checkpoint debt. History API
results bypass the rate policy so compressed backlog cannot be destructively
hard-filtered; they spend no live budget and trigger no queue flush, but retain
the ordinary canonical-ACK checkpoint requirement. Existing duplicate
suppression handles live/history overlap.

For one fixed outbound DM with no Zulip ingress, set `send_only: true`, omit all inbound fields, and configure exactly one `proactive_direct_messages` alias. This mode declares only scoped `zulip_send` without a tool group; sending uses `message` plus that sole alias and needs no receive designation. It never registers or polls a queue, publishes Zulip-originated events, installs reply/reaction authority, or activates an agent. Mode changes require extension restart.

In ordinary mode, the disabled tools are `zulip_register`, `zulip_conversations`, `zulip_send`, and separately tagged `zulip_react`; `tool_prefix` scopes all names and the text tool group. `zulip_register {}` means “make the authenticated caller this instance's receiving agent.” The legacy `{"enabled":true}` spelling remains accepted, while `{"enabled":false}` is rejected rather than pausing reception. Replies and reactions require opaque Tau-issued live references. Proactive sends require configured destinations; `zulip_send` accepts `topic` only for a discovered stream name explicitly marked `agent_chosen_topic`, and `topic: ""` is Zulip general chat. A proactive-DM alias sends only to its one configured recipient; callers cannot supply user IDs. Native stream, participant, message, queue, and credential values never become model authority.

### Ordinary send mentions

With an existing send grant, you can include mentions in authored Markdown,
including native `@**|123**` user mentions, without a separate mention grant or
configuration. The bridge preserves inline mentions. Mentioning someone does not
select a DM recipient or grant route, membership or send authority. Zulip policy and
notification settings determine notification effects.

For convenience, both ordinary and send-only `zulip_send` accept optional
`mention_users`: 1–32 strings, each at most 128 characters. Use canonical nonzero decimal u64 IDs
without leading zeroes, or locally resolvable `zulip-sender:` references from
configured normal/untrusted senders, the queue's bot identity and the bounded
retained participant locator queue. Use these values as locators, not mention
authorization: decimal targets may be outside sender tiers, including bots. The option adds no
network identity lookup, name guessing, grant, recipient selector, membership gate
or configuration. Send-only supports decimal IDs without a queue; opaque
references normally cannot resolve there.

The bridge prepends generated ID-only native tokens in caller order, preserving
duplicates. It separates tokens with spaces and adds a space before nonempty
authored text or captions. Authored Markdown, including inline mentions, remains
byte-identical. Omitting `mention_users` preserves existing behavior; an empty
list fails. Existing nonblank-text and attachment requirements stay unchanged.
Before provider effects, the bridge checks all targets and expanded wire/report
budgets, including attachment reservations.

In sent reports, the bridge replaces only generated prefixes: native wire tokens
`@**|123**` become presentation-only `@**|zulip-sender:<opaque hash>**` tokens.
These report tokens are not native syntax and do not claim to match the wire. Authored bytes
and attachment links remain identical in both bodies; the bridge does not
globally redact content or strip inline mentions. Failed or uncertain sends do
not publish confirmed sent reports. Existing frozen-route and lifecycle fences
remain unchanged.

### Unified sends and original files

Ordinary `zulip_send` accepts text, files or both through optional
`attachments: [{artifact, filename, content_type?}]`. The ordinary `zulip:send`,
exact scoped send grant, and Zulip group now authorize both text and uploads.
Review former text-only roles before upgrading. Remove obsolete
`zulip_send_attachment` and `zulip:attach` grants; there is no separate upload
tool or argument-level permission. Tools remain default-off.

The designated receiver supplies at most ten attachments and exactly one existing
`destination` or `reply_to`. Each `artifact` accepts a canonical Artifact autolink
or bare `blake3:` key, never a path, URL, inline bytes, `key` or `artifact_key`
field. Filenames are 1–128-byte ASCII basenames containing only letters, digits,
`.`, `_`, and `-`, without a leading dot. Unknown and duplicate fields fail closed.
`topic` retains existing explicit agent-chosen-topic authority.
Omitted or empty attachments require nonempty `message`; nonempty attachments allow
an omitted or empty `message`. After exporting a PNG, substitute its actual digest
and an authorized destination in this example:

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

The digest and destination above are illustrative, not usable authority.
Do not pass a local path or substitute a native Zulip destination ID.

Optional `content_type` must be
exactly one of `image/avif`, `image/gif`, `image/heic`, `image/jpeg`,
`image/png`, `image/tiff`, or `image/webp`, and becomes the multipart file-part
`Content-Type`. When it is omitted, the file part remains
`application/octet-stream`. It is an unverified caller declaration: the
extension does not infer it from the filename, Artifact metadata, or artifact
bytes, or validate the bytes against it. A non-string or unsupported value
fails before Artifact RPC or provider effects. Use `image/png` for a generated
PNG. The outbound Markdown remains the same `[filename](/user_uploads/...)`
link. A supported image MIME can let Zulip, its server, or clients process a
preview, but does not force inline display.

The tool validates all metadata first, then verifies and uploads originals
sequentially, retaining only one original at a time. Each file is at most 16 MiB,
not a 16 MiB aggregate group limit. The configured message limit can admit fewer
files because the tool reserves space for every maximum-size upload link before effects.
One final Markdown message contains all links on the frozen route.
The tool accepts files as well as images; Zulip selects previews and can impose
a lower file-size limit. The tool never
accepts local paths, raw bytes, remote URLs, inbound downloads, public temporary
links, or harness-store access. The sent fact preserves the exact outbound
Markdown, including an ordinary authenticated relative upload link as inert
content, never as recipient or reply authority.

One send, import or profile transfer holds shared transfer admission at a time.
Each original has a 60-second Artifact deadline; provider operations retain
30-second HTTP deadlines. Sharing does not renew Artifact retention:
existing originals persist independently
of ephemeral transcripts and can become unavailable. Upload and send are
separate effects, so failure or cancellation can leave an orphan upload or an
uncertain sent message. Transfer errors report confirmed-upload count and whether
the current upload may exist; no message was sent. Final-send failure may have an
unknown message outcome after all uploads. Cancellation, retirement or local
publication failure can leave remote effects; never claim rollback or retry
automatically. Inspect the conversation first. Success retains `status: "sent"`,
`message_ref`, and `delivery_copies: "one"` with the exact outgoing Markdown.
Send-only mode stays text-only and rejects nonempty attachment lists.

A separately attachment-read-capable extension revision adds
`zulip_list_attachments` and `zulip_download_attachment` in ordinary
receive mode. An `std-zulip` instance exposes them only when its installed
`tau-ext-zulip` implements that read contract. They share the
`zulip:attachments:read` tag and have no tool group, so a role must grant each
scoped tool explicitly. After listener registration, an agent can list at most
16 standard Markdown attachment links from one message delivered to that same
agent by passing the delivery's opaque `message_ref`. Listing returns bounded
untrusted display names and source-bound opaque `attachment_ref` values, never
private Zulip paths or native message IDs.

`zulip_download_attachment` takes that same `message_ref` and one returned
`attachment_ref`. Listing and download each refetch the exact current source
message under the bot's configured access and receive policy. Download also
rechecks listener registration, source ownership, and selected attachment
around remote I/O before importing the exact bytes into Tau's shared Artifact
namespace. Its result includes the untrusted name, verified size, and canonical
`blake3:<64 lowercase hex>` Artifact key. That key is directly compatible with
`zulip_send` attachments; the core-shell `import` tool can also materialize it
for filesystem reading or other local work. It grants byte lookup, not Zulip
reply, recipient, sender, or route authority. Neither read tool scans message
history. Accepted edits replace the selection; moves, access loss, deletion,
failed refresh or attachment-budget pressure, owner eviction, registration or
configuration retirement, and restart can retire it. The 16 MiB Artifact limit
applies.

The extension emits generic message reports for creates, edits, deletes,
reactions, and successful sends. Edits, reactions, and deletes with a supplied
actor require a top-level numeric actor in the union of `allowed_user_ids` and
`untrusted_user_ids`. Zulip's singular delete may omit its actor only when its
top-level message ID, message type, and stream/topic fields match one exact current
source owner; the checked report records no actor and successful publication
revokes only that owner. Bulk, nested-ID,
incomplete, contradictory, unknown-owner, and stale-authority deletes fail
closed. `offline_message_catch_up` defaults to false, preserving live-only
reconnect behavior. When enabled, it registers a fresh live queue, retrieves
bounded created-message history after an identity-scoped durable checkpoint,
merges/deduplicates the live overlap, and advances only after its canonical
delivered fact returns on the post-persistence downpath. First use establishes
the current baseline without replay. Offline edits, deletes, and reactions are
not recovered; filter changes do not rescan before the checkpoint. Crash
recovery is at-least-once and can duplicate messages. Runtime reply/reaction
references disappear on restart. Admitted Zulip Markdown remains exact through
canonical facts, replay, and provider context, including a leading addressed bot
mention.

Zulip topics are conversation threads, not exact parent-message links.
Quote-and-reply inserts editable Markdown attribution, a message link, and quoted
text. The bridge preserves that exact body on admitted creates and accepted edits.
The optional activity summary can precede a created message's unchanged body.
Quoted attribution and links remain unverified external content and grant no
sender, admission, route, or reply authority. The bridge does not extract or fetch
a native parent from quote Markdown. `zulip_send(reply_to)` selects the live owned
source topic or DM participants, not a native per-message reply target.

### Room participants

`zulip_list_room_participants {"message_ref":"zulip-message:…","limit":20,"offset":0}`
requires the separate default-off `zulip:participants:read` grant, outside the text
group. Call it as the current receiver in ordinary mode. The reference must
identify a currently owned admitted inbound message; historical reads and
proactive sends do not create this authority.

Stream sources require whole-stream receive authorization, not a topic-only grant.
Their `basis: "stream_subscribers"` means active subscribed humans and bots, not
topic followers, everyone who can read or people who posted. DMs use
`direct_recipients`: the admitted source's exact recipients, including self.
Membership is not presence or live account status.

`limit` defaults to 20 and accepts 1–32; `offset` defaults to zero and accepts
0–131072. Continue with the returned `next_offset`, which is null at the end.
Opaque IDs are sorted before paging; self need not appear on every page. `complete` means the
page reaches the end, not an atomic snapshot. Past-end offsets return an empty
complete page. Every stream call fetches membership anew, so changes can repeat
or skip rows.

Rows contain opaque `user_id`, `is_self`, nullable `display_name` and `is_bot`,
and `name_state`. Names are untrusted and bounded to 512 UTF-8 bytes.
Registration feature level 384 or newer enables one selected-user name batch
for the page. Older or unknown servers return `name_state: "unsupported"`;
missing or failed names return `"unavailable"` without invalidating membership.
There is no directory fallback, per-user fan-out, presence lookup or avatar import.
Each response is capped at 256 KiB; an oversized roster fails even for a small
page. At most two requests share a ten-second budget, with five seconds per request
and no redirects or retries.

Successfully submitted rows can support independently authorized
`zulip_get_user_profile` through a receiver-scoped queue of at most 256 locators.
Eviction, expiry, receiver retirement, reconfiguration or restart can invalidate
them; provider visibility and `zulip:profiles:read` remain necessary.
Listings emit no canonical facts and grant no sender admission, source ownership,
send authority, routes, subscriptions or activation.

### User profiles and automatic avatars

`zulip_get_user_profile {"user_id":"123"}` is default-off, tagged
`zulip:profiles:read`, outside the ordinary group and absent in send-only mode.
It requires the current receiver and queue identity. Canonical nonzero decimal
native u64 IDs permit lookup of API-visible realm users or bots independently of
ingress sender tiers. Known opaque `zulip-sender:…` references work for configured
sender tiers, self, or retained receiver-scoped participant locators; unknown
opaque IDs cannot trigger directory search. Results expose only opaque identity
and bounded untrusted metadata.
Lookup grants no admission, message facts, source ownership or route authority.

Profiles include available name, timezone identifier, join date, account state,
bot type, role and supported custom fields (up to 16 bounded fields, without
rendered HTML or user-reference values). Status distinguishes available, none,
unsupported and unavailable. Bots/inactive accounts have unsupported
status/presence. Optional failures preserve useful base profile data. Email,
owner/native-reference fields, raw provider URLs/errors and arbitrary response
objects are omitted.

Presence exposes provider-reported active/idle/offline and timestamps when
available, not inferred current state. Account activation is not online
availability. Timestamps may be stale or use account join date as a compatibility
fallback; never describe them as verified last-seen times. Missing or inaccessible
data is unavailable, not offline.

The tool automatically imports eligible avatars unchanged into Artifact storage.
`avatar.state="imported"` returns canonical reference, verified size and MIME
hint. Missing avatars are `none`; rejected custom CDN origins are `blocked_origin`,
not absence. Other states distinguish `unsupported_url`, `unavailable`,
`too_large`, and `unsupported_media_type`. Only narrow configured-origin
`/user_avatars/` and `/static/` paths or the official HTTPS Gravatar hash path are
supported, without credentials or redirects. The limit is 2 MiB; PNG/JPEG/GIF/WebP
MIME/signature checks do not decode images or prove safety. Transfers share
attachment admission. Cancellation may leave committed artifacts, which persist
independently of transcripts. Source availability neither changes configuration
nor activates a live bridge.

### Editing the bot's own avatar

`zulip_update_self_profile` edits only the authenticated bot's avatar. Supply
`{"avatar":"<tau-artifact:blake3:…>"}` to upload; a bare canonical `blake3:…`
key also works. Use `{"avatar":null}` to reset to the realm default, which may
not be blank. Other profile fields stay unchanged.

The separate default-off `zulip:profiles:write` grant is outside the text
group. The tool requires the current ordinary receiver and is absent in
send-only mode. Read/send grants do not authorize avatar edits.
Obtain explicit user authorization before changing grants or live configuration.

The bridge rejects empty patches, unknown fields, target IDs, URLs, paths and
inline bytes. Ordinary bot credentials cannot edit names, timezones, statuses
or custom-profile maps. Incoming-webhook credentials cannot use this avatar
endpoint, and realm policy can reject edits.

The bridge verifies original Artifact bytes and receives the Close
acknowledgement before upload. It accepts PNG/JPEG/GIF/WebP signatures up to
2 MiB and ignores filename/MIME hints. Signature checks do not prove image safety.
The provider may enforce a smaller limit or transform the image.
Each call makes one upload or reset without automatic retry or rollback.
An unknown outcome may mean the avatar has changed. Results omit raw avatar
URLs.

### Linked messages

Enable `zulip_get_message` (`zulip:messages:read`) separately, then use
`{"message_id":"<message_ref or linked ID>"}` to recover current permitted content.
It is the eighth ordinary-mode tool, outside the text group, and absent in
send-only mode. It requires the current designated receiver and checks current
receive routes, author tiers, and admission of every DM participant. The
authenticated bot's messages are readable. `mentions_only` controls live delivery
and activation, not explicit historical reads.

Results contain `message_id`, descriptive `author`, untrusted `text`,
`content_trust: "external"`, and optional `previous_id`. Follow only the IDs you need;
there is no search or history-list interface. `previous_id` means the immediate
provider predecessor in the exact same topic or DM, not the last delivered
message. The bridge omits denied or unproven predecessors rather than substituting
an earlier message. Zulip provides no reliable native branch-root or reply evidence:
`root_id` and `reply_to_id` remain absent. Editable quotes and oldest-visible
history establish neither.

With `ingress_rate_limit`, delivered facts omit previous links to avoid delaying
rate accounting; retrieve the delivered message by its own ID for on-demand links.
Edits do not inherit links. New versioned opaque IDs survive restart, compaction, and owner-cache
eviction while the managed identity key is unchanged; rotation invalidates them.
Legacy hash-only IDs are not reversible and cannot be recovered.

IDs are not capabilities. Each read checks current policy and returns current
provider content, not an archive. Reads may fail after deletion, access loss, or
policy changes; failure alone does not prove deletion. Reads create no delivered
facts, activate no agent, and grant no reply, reaction, send, or attachment
authority. Existing live-owner requirements still govern writes.

Configure receive selection per ordinary-mode extension instance:

```yaml
register_on_start: true # register extension automatically
role: coordinator # role to deliver to; optional, constrains registration and automatic delivery
```

`register_on_start` defaults to `false`. That default restores an eligible saved
manual designation after restart, but never first-selects or creates a receiver.
With `true`, an eligible saved designation remains sticky; otherwise the bridge
selects the oldest eligible existing agent. `role` is optional, constrains both
immutable creation-role eligibility and `zulip_register` callers, and is required
for lazy creation. Creation can follow only actually admitted external input.
Without a role, automatic mode selects existing agents only and never guesses a
role. Unknown roles and unreadable designation snapshots fail closed. An unloaded
saved ID is never force-loaded.

### Opt-in canonical-main receiver

The reviewed SDK 0.20.0 / protocol 16.5 adoption also accepts top-level
`target: canonical_main`; omission defaults to the ordinary selection described
above.
With `register_on_start: true`, admitted input can Ensure the canonical
main through prompt-free construction without an explicit `role`. An optional
role still constrains immutable creation-role eligibility. Every Register,
Restore, Select, and Ensure uses the targeted SDK helper and Configure's harness
protocol revision. Missing revisions, revisions below 16.5, and other major
versions fail closed before sending; the bridge never silently downgrades.
A saved non-main Restore fails without rewriting the designation or granting
authority. A pending or unavailable main does not authorize an ordinary substitute.

Receiver targeting is separate from outbound tool restrictions. Harness
`tool_policy.canonical_main_only` selects exact final registered internal tool
names, not roles, tags, groups, aliases, or dotted instance names. Normal tool
grants remain necessary. For a prefixed instance, use its actual
`<tool_prefix>_<local_tool_name>` inventory. The unprefixed locals are
`zulip_register`, `zulip_conversations`, `zulip_send`, `zulip_get_user_profile`,
`zulip_list_attachments`, `zulip_download_attachment`, `zulip_react`,
`zulip_get_message`, and `zulip_list_room_participants`. Audit the listed inventory
and explicit main-only tool/veto policy before a source bump or read grant; the new
read tool is outside the text group and needs its own authorization.
Same-role children and ordinary parentless agents do not inherit canonical-main
authority. Nix owns the dedicated profile, credentials, participant and route
restrictions, and outbound policy; this feature alone does not restrict DMs to
one person or activate a live bridge.

The extension saves one versioned concrete receiving-agent designation per
persistent session and configured instance; ephemeral sessions retain it only in
memory. A same-agent registration is an inert success. A different eligible
caller replaces the designation only after its snapshot saves. An actual handoff
may send one best-effort fixed notice to the previous receiver only while it is
loaded; there is no initial-selection or replay notice. An absent snapshot may
use one unambiguous accepted historical registration as migration evidence, but a
saved designation wins over that history.

Already reported input and its ACK/checkpoint ownership remain with the original
agent. Handoffs do not transfer old source, reply, attachment, reaction, queue,
or write authority. Send-only mode and separately granted send, reaction, and
attachment tools remain independent. `offline_message_catch_up` remains opt-in
and is the only way to request bounded created-message history.

## Diagnose receive restoration and queue recovery

Zulip writes content-free diagnostics to its supervised extension stderr, which
Tau captures in the extension log. By default, `zulip=info,warn` makes the
extension's `info` and higher records visible, and leaves unrelated targets at
`warn` and higher. `TAU_LOG` can override that filter; set it in the environment
that starts the harness, then restart the harness or session so its new
extension child inherits it, as described in
`tau-self-knowledge-debugging-extensions`.

Each delivered replay boundary records an `ok` or `error` outcome with
content-free gate flags or bounded counts. Receiver-resolution rejections expose
only stable typed categories, never arbitrary downpath text. Receive-restoration
warnings use the `receiver_resolution` or `restore_worker_spawn` category;
queue-setup failures use `queue_setup`. Queue expiry, replacement success,
malformed event batches, and long-poll failures remain separately visible.

These records contain no message bodies, credentials, headers, queue or native
IDs, routes, or raw remote error bodies. A positive record shows that the
extension reached that point; an absent record is inconclusive unless the
filter and log capture are independently known complete. Logging is best-effort
operational evidence, not journal authority or a live probe: it neither
preserves attempts across process loss nor establishes or fixes the root cause
of a historical registration failure. In particular, absent restoration
records can mean that no replay subscription supplied the necessary durable
intent (as with older live-only startup behavior), not necessarily that a
Zulip API request failed.

Queue-poll failures retain content-free classifications for bounded body-read,
JSON, result-envelope, queue, and events-shape failures. Startup and
re-registration failures retain only the operation, HTTP status, and a bounded
uppercase Zulip machine code. These diagnostics expose no response bodies,
credentials, queue IDs, native IDs, routes, or message content. They classify
future failures but do not prove the exact subtype or root cause of any past
live incident.

The separately maintained `tau-ext-zulip` project owns the complete operational,
security, testing, architecture, and routing documentation.
