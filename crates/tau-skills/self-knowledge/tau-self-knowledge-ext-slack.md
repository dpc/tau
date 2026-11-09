---
name: tau-self-knowledge-ext-slack
description: Use this extension skill for Tau std-slack setup, conversation policies, Slack Socket Mode scopes/events, tools, routing, security, edits, reactions, dynamic DMs, or troubleshooting.
advertise: false
---

# Tau std-slack extension self-knowledge

`std-slack` is Tau's disabled-by-default Slack Socket Mode bridge. It exposes
`slack_register`, `slack_conversations`, `slack_send`, `slack_react`,
`slack_get_message`, `slack_list_attachments`, `slack_download_attachment`,
`slack_get_user_profile`,
`slack_list_room_participants`, `slack_list_rooms`, `slack_join_room`, and
`slack_recent_history`. `tool_prefix` scopes all twelve tools and the `slack`
group. Every Slack tool belongs to that group, including incoming attachment
reads, profiles, room participants, public discovery/join and recent history.
Use `enable_tool_groups: ["fedi_slack"]` with `tool_prefix: "fedi"` to include
all present and future tools in that instance. Tools remain default-off; narrow
exact-tool and semantic-tag grants remain available. Exact scoped exclusions such
as `disable_tools: ["fedi_slack_join_room"]` apply after group grants, but explicit
`enable_tools` entries can re-enable them. Existing whole-group grants gain tools
that were previously outside the group; review those grants before upgrading.
Grouping changes no receiver, route, sender, installation or OAuth policy.
Slack text is always untrusted external content. This source uses published Tau
SDK 0.29.1 and protocol 23.1. It requires a protocol-23-compatible harness.
New original and avatar imports use anonymous metadata-bound artifact identity;
outgoing originals verify complete descriptors, including historical payload-only
objects. Local byte limits and tool grants remain unchanged.
This source change does not install or activate a bridge.
The Hello greeting reports the Slack package's own release and source revision,
not the SDK or harness version. Supporting harnesses show the ready instance's
identity in `self_info`. Git-free archives without a package stamp report an
unknown revision; tracked source changes add `-modified`. Build identity is diagnostic metadata,
not authentication or proof of installation or activation.

## Tool display metadata

The send, configured-route inventory, public-room list, join, recent-history and
exact-message-read tools use shared themed display metadata. They show counts in
native labelled counters rather than prose in the argument label. Expanded
summaries omit message bodies, locators, attachment names and artifact references.
Inventory summaries show at most eight rows with bounded, escaped labels; the
structured tool result retains full pages.

These tools show warnings for uncertain membership, an unavailable latest thread
window, uncertain file sharing and possible duplicate text delivery. They show an
error for unshared files without changing the structured result, and omit unknown
attachment counts rather than showing zero. Displays identify provider pagination
and filtered or omitted history; empty output never proves empty history. These
display changes do not alter grants, retries, delivery or activation.

Compact rows omit redundant static labels and routine zero attachment counts;
full summaries still distinguish zero from unknown. Initial progress omits
argument labels because argument validation has not yet run.

## Public room discovery, join and recent history

Enable the whole Slack group, or grant each exact scoped tool or its tag:
`slack:rooms:read`, `slack:rooms:join`, `slack:history:read`.
These capabilities remain default-off; inventory/send grants alone do not enable
them. All require current registration and an authenticated installation;
no configuration keys change.

`slack_list_rooms({"limit":20})` lists one provider-visible page of public channels
that are not archived, including rooms absent from local configuration. The limit
accepts 1–50. Records
contain an inert native `room_id`, untrusted `name` and `is_member`, but no topic,
purpose, people or messages. Follow `next_cursor` explicitly as `cursor`.
Opaque continuations bind to the original agent and lifecycle, expire after 15 minutes
and share a 128-entry FIFO. Empty pages need not be final; `complete` means
the provider exhausted pagination, not that the results form a snapshot.
Configured aliases remain in `slack_conversations`.

`slack_join_room({"room_id":"C123"})` verifies that an existing channel is public
and not archived, then joins it once. Independently known native C IDs work too;
no ephemeral
per-room grant is required. This never creates rooms, invites users, joins private
channels/DMs/MPIMs, or configures local receiving/sending. `joined` confirms
provider membership; `join_unknown` means membership may have changed. Never
automatically retry an uncertain result. Cancellation or retirement can leave
remote membership; there is no compensating leave. A non-evicting 1,024-call
process-local set prevents repeat attempts under the same call ID.

Room failures carry bounded `stage` / `reason` labels and an action.
`list_fetch` and `info_fetch` identify provider fetch failures; `list_validation`
and `info_validation` identify strict public-room/page validation failures.
Examples include `missing_scope`, `permission_denied`, `transport_timeout`,
`malformed_response` and `unavailable_or_policy`. These labels describe observed
failures without diagnosing the operator's underlying configuration. During
preparation, the bridge reports `prepare` / `unavailable_or_policy` without
revealing which private admission prerequisite failed. When the budget expires,
it reports `deadline_exceeded` at the selected boundary
(`list_fetch`, `info_fetch` or `join_preflight`).
A failed remote join attempt still returns `join_unknown`; its existing `note`
adds `join_attempt` and a safe reason. Do not treat that reason as proof that
membership did not change or retry on that basis, even for permission or
authentication failures.
Retirement replaces pending provider facts with `disclosure` /
`lifecycle_changed` and preserves remote-membership uncertainty.
No raw provider codes, headers, bodies, private IDs, URLs or token data enter
these diagnostics. Use retained results and existing operator logs;
there is no status/auth/scope probe tool, no new logging, and no permission
to broaden OAuth grants or change receive/send policy. Older generic room
errors do not identify a cause; check the deployed build before relying on
these labels.

`slack_recent_history({"destination":"team","limit":10})` requires a configured
receive-enabled alias, not outbound permission. The limit defaults to 10, with a
maximum of 50.
Optional `thread_id` is a matching root message locator. Fixed-thread aliases
use their configured root and reject others. Whole-room history reads one
newest-first provider page. Exact-read sender policy filters the selected window
without scanning to fill it; fewer or zero visible messages do not prove empty
history. Successful output includes `order:"newest_first"`, `page_filtered`,
`provider_page_exhausted` and `older_items_omitted`.

Thread replies are oldest-first. The tool returns newest thread messages only if
the provider confirms exhaustion in one page of at most 50 items. Otherwise it
returns
`latest_window_unavailable`, not the oldest page mislabeled as recent. This
implementation bound is not a supported maximum total thread size. No automatic
scan, cursor mode or arbitrary time window exists. Exact `slack_get_message`
reads of known references remain available.
History is untrusted context, installs no source/write authority and retains no
profile mappings; use exact reads to refresh profile locators separately.

Discovery requires `channels:read`; join adds `channels:join`. Operators must
review grants, update scopes and reinstall separately. History needs the existing
conversation-kind history scope and identity verification (`users:read`); native
permissions and rate limits still apply. No new Slack events or OAuth changes
are automatic. All work shares eight read slots and bounded response/output
bytes. History starts no further work after 30 seconds; an in-flight request has
its own 30-second timeout. Retirement suppresses late disclosure.
The source change does not install, activate or alter live membership.

## Room participants

The separate default-off `slack_list_room_participants` tool requires
an exact-tool grant, `slack:participants:read`, or the whole Slack group.
Call `{"destination":"team","limit":20}` with an exact configured
receive-enabled whole-room alias. It rejects fixed-thread-only, proactive-only,
dynamic-only and native-ID routes. Current registration and installation are
required. Strict mode returns allowlisted canonical accounts and exact self;
lax mode returns API-visible members. Each call reads one `conversations.members`
page of up to 20 provider IDs by default, with a maximum of 32, and verifies
accounts before returning minimal identity metadata.
Results use `basis:"conversation_members"`, opaque profile-resolvable `stable_id`
and optional names, `is_bot` and `deleted`; missing fields mean unknown.
No avatars or presence are fetched.

Follow `next_cursor` explicitly by passing `cursor` with the same destination.
An empty filtered page is not an empty-room claim. `page_filtered` applies only
to that page; `complete` means the provider has exhausted pagination for permitted
visibility. It does not promise a consistent snapshot, full unfiltered roster
or unique names. Provider or identity verification failure returns an error,
not an empty successful roster. Results remain untrusted external content.
Cursors expire after 15 minutes. Eviction from the 128-entry FIFO, restart or
loss of the original agent/configuration/installation/session lifetime also
invalidates them.
Disclosed profile locators use existing bounded same-agent retention.
Discovery confers no profile grant, history, write, mention or control authority.

Provider prerequisites are `channels:read` for public channels, `groups:read`
for private channels, `im:read` for DMs or `mpim:read` for MPIMs, plus
`users:read` for account enrichment. Existing profile/send grants do not prove
these scopes exist. Missing scope fails closed; no automatic OAuth changes,
grants or workspace scans occur. Work shares eight read workers, a 30-second
total deadline and finite response/result bounds.

## Profile lookup

Enable the whole Slack group, or grant `slack:profiles:read` or the exact scoped
`slack_get_user_profile` tool.
Input contains only `user_id`, for example `{"user_id":"U123456"}`. It accepts
U/W accounts (including bots/apps), B workspace bot IDs, or retained same-agent
opaque `slack-sender:…` references. A-prefixed app IDs are not account selectors.
Native IDs work without a cache. Opaque references resolve through current
canonical live ownership or a 2,048-entry FIFO of historical/profile and native
mention locators, scoped to the original agent and lifecycle.
Restart, eviction or retirement can invalidate them; use authorized
`slack_get_message` on a durable message ID to refresh its author mapping.
For fresh same-agent creates and owned edits, the bridge retains the bounded
projected native people mentions only after exact canonical confirmation, even
if optional names are unavailable. Pending reports and canonical replay install
no locators. A mention locator does not verify identity or authorize disclosure;
every profile request still applies current policy and exact provider identity
verification.
The bridge uses no directory scan, search, hash reversal or persistent index.

The caller must be registered with a current authenticated installation. Strict
mode requires an allowlist entry for the canonical mapped U/W (or unmapped B)
principal, except for the exact bridge identity. Lax mode admits directory
targets visible to the installed token. Profiles grant no history, message write,
control, dynamic-link or sender admission authority.

Optional fields include names, title, pronouns, email/phone, locale,
timezone/offset, custom status text/emoji/expiry, deletion and account/bot/app/
admin/owner/guest attributes. Output keeps native IDs and avatar URLs private.
It includes `stable_id`, `content_trust: "external"`, `account_profile`,
`presence` and `avatar`. Presence is `active`, `away` or `unavailable`, never an
inferred offline/last-seen value; `always_active` stays a separate bot hint.
Unavailable optional enrichment does not discard the base profile. Missing
fields are unknown, not false; metadata and imported bytes remain untrusted.
Custom fields and DND/snooze are not queried, and UI parity is not promised.

All three API methods (`users.info`, `bots.info`, `users.getPresence`) use the existing
`users:read` scope. Email requires an already-granted `users:read.email` scope;
it is optional, not a setup requirement. Profile lookup requires no new event
subscription or SDK change.

Supported avatars import automatically as original bytes through Artifact RPC.
`avatar.status` is `available`, `not_provided`, `unsupported_origin` or
`unavailable`. Available results include canonical `artifact`, bare `key`,
`size` and unverified `mime_type`; inspect via a separately authorized image
tool. The tool prefers the provider's original, otherwise its largest standard
rendition. Transfers are credential-free, limited to 4 MiB/10 seconds, share the
single binary slot with attachments, and reject redirects and encoded responses.
Supported HTTPS origins are exact `avatars.slack-edge.com` and narrow
`secure.gravatar.com/avatar/<hash>` paths with bounded size/default queries.
Other CDN hosts and default-avatar redirects are unavailable, not arbitrary-fetch
fallbacks. See the [profile boundary](../README.md#user-profiles-and-avatars).
Artifacts persist independently of ephemeral transcripts; interrupted import
may leave undisclosed bytes. Profile calls share eight read workers, with
bounded directory requests. Final disclosure serializes with lifecycle retirement.

The built-in Tau copy of this skill must be synchronized separately; source
support is not evidence that a live installation has the required grants.

To receive at startup without an LLM prompt or registration call, add
`register_on_start: true` and `target: canonical_main` to the existing instance
config. Keep its secrets, allowlist, and conversation policy. `role` is an optional
immutable creation-role constraint. Defaults are automatic registration off,
ordinary target, and no role constraint. Startup uses Select only; fully admitted
plain input may use Ensure to create main without an explicit role or bootstrap
prompt.
Canonical-main targeting requires advertised protocol 23.x,
never selects an ordinary substitute, and cannot replace an unavailable bound main.
Configured manual registration uses exact harness caller validation with the same
constraints. Automatic mode rejects unregister; disable it in configuration and
restart. Default manual behavior is unchanged.

Slack uses the shared resolver but not the persisted sticky-designation package.
After restart, Slack selects the harness-owned main again; ordinary selection may
change, and manual registrations do not persist. Selection grants no outbound permission or
source transfer. First input retains its bounded FIFO slot during event-driven
readiness retries for at most 30 seconds, with five-second RPC bounds.
Timeout/rejection submits no message; existing post-ACK loss and process-local
duplicate suppression still apply.

For development, `just policy` keeps intended package-root files root-anchored
and rejects Cargo package membership beneath `.direnv`.

Configuration requires app/bot token secrets, nonempty exact U/W/B
`allowed_user_ids`, and an active `conversations` and/or
`dynamic_direct_messages` policy. Each `conversations` item binds a stable alias
to an exact C/G/D conversation, explicit `channel`/`mpim`/`dm` kind, optional
fixed root `thread_ts`, optional `receive: mentions_only|all_messages`, and
independent `proactive_send`, plus an optional operator-authored description on
any active static record. Native ids and raw thread selectors are never separate
write-route arguments; read locators encode recoverable native coordinates. `channel_ids`,
`listening_scope`, and `send_destinations` were removed and are hard errors.
Follow the [README migration procedure](../README.md#migration-from-removed-keys).
Optional `sender_aliases` bind at most 64 exact U/W ids one-to-one to unique
lowercase aliases. Native U/W identity remains extension-local authority.
Model-facing facts use an installation-scoped opaque sender reference; a
configured alias takes display precedence over the bounded
`profile.display_name`, and both are untrusted presentation only.

For inbound exact mentions of the authenticated installation bot, exactly one
eligible leading mention is removed for routing/command compatibility and
remaining eligible mentions become the semantic token `@slack_bridge` in
published text. Complete
equal-length backtick code ranges suppress recognition. Escaped, labeled,
partial, case-changed, and literal `@slack_bridge` text do not count. Successful
registration returns exactly
`{"status":"registered","incoming_transport_reference":"@slack_bridge"}`;
unregister returns `{"status":"unregistered"}`. The token discloses no native
id, grants no authority or capability, and does not expand egress; outbound
`@slack_bridge` remains literal text.

Delivered and edited facts also carry optional descriptive mention evidence.
Native outer references use the same installation-scoped opaque identity as
senders. They include optional bounded names (including other bots) and a
comparison with the authenticated bridge identity. A stripped leading self
mention retains its comparison without a source span; retained spans point into the exact final
body. Code, prose blockquotes, and appended attachment quotes do not contribute.
Each edit supplies fresh evidence; replay does not resolve names again.

Mention evidence describes references; it does not invite an answer. Admission, delivery,
activation, and an available reply selector do not mean the message addresses
this agent. `is_self` compares with the bridge account, not a particular Tau
agent. Missing evidence or a missing boolean means unknown; a partial people
list never proves absence. Optional name lookup accepts bots without changing
sender admission, requires no new Slack scope, and stops on failure without dropping
the message. The bridge makes at most eight optional lookup requests within a
total two-second budget per report.
Both bridge and harness must support this metadata; older harnesses may ignore
it. The built-in Tau skill copy must be synchronized separately.

The SDK also supports `reply_context` for native quoted-parent context, but this
bridge does not emit it. The bridge emits typed `MessageLinks` root links instead
of the descriptive thread-root suffix; mention spans refer only to the normalized
outer body before quotes.

Dynamic DMs are opt-in, bounded to 64 exact D-to-allowlisted-U/W links,
receive-only, multi-link, and runtime-only. Static receive DM policy wins over
dynamic discovery; proactive-only static DM policy does not block it.

`strict` admits exact allowlisted Slack-verified senders. `lax` also admits verified
senders on static routes, but not DM linking or control. Other bots/apps, including
verified B-only integrations, enter live ingress only on static `all_messages`
routes. The legacy `allowed_user_ids` key accepts U/W/B sender IDs: use the
canonical U/W when `bots.info` supplies a verified mapping, otherwise the exact B.
Application IDs (A) and names never authorize senders. `sender_aliases` remains
U/W-only. Bot text is ordinary untrusted prompt content, never human-only
commands, dynamic linking, inbound reactions or `mention_source_user`.
The bridge excludes its exact authenticated U/W and B identities, not every bot
using the same application. Independent bots can still form reply loops.
`mentions_only` means
Slack `app_mention` delivery. Outside DMs, commands
require a leading authenticated bot mention; duplicate `message` and
`app_mention` wrappers normalize identically. Parent routes include all threads;
receive-enabled fixed-thread routes isolate selection and normalize the root create.

Every setup needs `connections:write`, `users:read`, and `chat:write`. Add:

| Surface | Events | Scope |
|---|---|---|
| Mentions in channel/private/MPIM | `app_mention` | `app_mentions:read` |
| Public all/edit | `message.channels` | `channels:history` |
| Private all/edit | `message.groups` | `groups:history` |
| MPIM all/edit | `message.mpim` | `mpim:history` |
| DM | `message.im` | `im:history` |
| Inbound owned-post reactions | `reaction_added`, `reaction_removed` | `reactions:read` |
| Optional agent add/remove reactions | none | `reactions:write` |
| Optional outgoing artifact attachments | none | `files:write` |
| Optional incoming original reads | existing message events | `files:read` plus current conversation-history scopes |

Reinstall after scope/event changes and invite the app to every route.
`chat:write.public` is unnecessary. DMs never use `app_mention`.
`auth.test` must return the bot U/W and B identities and installing T workspace identity.
Supported wrappers require matching `context_team_id` or one unambiguous
matching authorization; missing/mixed/mismatched evidence is dropped before
identity lookup. Slack Connect actor home team is not installation authority.

Agents register to receive unless startup registration is enabled. Accepted Slack creates, edits, deletes, and reactions
submit transient `message.*_reported` events; the harness publishes immutable
canonical `message.*` facts downstream. `slack_send` uses a configured outbound
`destination` with optional `reply_to` or `thread_id` locators, or a `reply_to`-only
shorthand resolving an ordinary outbound grant. It requires a nonempty
`message` only when `attachments` is absent or empty. Successfully
published creates and edits install optional source metadata and mutation ownership only after their
exact canonical facts return to the configured publisher's live downpath. Each
report carries a stable opaque occurrence ID. Deletes revoke authority immediately
and remain pending until the same canonical confirmation. Socket Mode ACK is
separate from Tau commit. Edits require a known canonically confirmed original;
reactions require a recent Tau-authored post and covering
receive policy. All agent sends need no registration but still require live
extension/session authority and effective tool policy.

Forwarded/shared message previews supplied inline in the same Slack event now
reach agents as labelled untrusted attachment quotes, with or without an outer
note. Supported forms are attachment `text`, textual `blocks`, nested
`message_blocks[].message.blocks` compatibility previews, and `fallback` when
no richer body exists. The original author is not independently authenticated;
the verified sender/editor in retained source metadata remains the forwarder.
Replies require configured outbound grants. Agents may use
quoted content under that sender's request, but quotes do not control bridge
commands, mentions, routes or identity.

Inline attachment extraction never retrieves linked messages, arbitrary URLs,
files, or images.
Slack's “Only show link” choice, inaccessible content, missing inline text, and
unsupported blocks cannot recover the original body. With a note or another
usable quote, missing inline content is marked explicitly; an attachment-only
forward needs usable text. Separately, valid file observations can produce a
count-only bridge-authored notice, including otherwise-admitted file-only messages.
The notice remains untrusted body content, never a mention, command or independent
sender. The bridge preserves captions; the notice does not assert current
readability.
Extraction is bounded and rejects malformed/oversized
content instead of truncating; the final comment-plus-quotes must fit the
configured message limit. No new Slack scopes, configuration or tools are needed.

### Outgoing original files

`slack_send` accepts optional `attachments` and requires bot OAuth `files:write`
for uploads. **Permission broadening:** `slack:send`, exact scoped send grants and
the ordinary Slack group now permit both text and files. Remove the obsolete
`slack_send_attachment`/`slack:attach` grants and review former text-only roles
before upgrading. SDK 0.21 cannot enforce an independent upload grant on this
single tool; no duplicate tool or upload configuration switch is retained.
Add the scope and reinstall/re-authorize separately; source support
does not prove live OAuth permissions. No new events or `files:read` are needed.

Provide `attachments: [{artifact, filename, content_type?}]`, with at most ten
entries and 16 MiB total original bytes. Prefer a bare 24-lowercase-hex key for
`artifact`. You can also pass `tau-artifact:KEY` or the Markdown link
`<tau-artifact:KEY>`. Use brackets for Markdown/context presentation; tool
arguments do not require them. Slack rejects deprecated full
`blake3:64-lowercase-hex` keys, bare or wrapped.
The larger shared SDK object ceiling does not enlarge Slack's limits.
The tool rejects unknown fields. Use the same outbound destination and optional
reply/thread selectors as text sends. Omitting `attachments` or passing an empty list preserves
text-only behavior. A nonempty list permits an omitted or empty `message`; text
becomes one initial comment on the group, not a separate text post.
`mention_source_user` keeps its reply-only
verified-human restriction. The tool uses current text-send route authority and
preserves the authorized thread root. It verifies all originals through Artifact
RPC and checks the aggregate limit before any Slack upload. It accepts no paths,
URLs or inline bytes and does not generate images. Generate and export a file
with separately authorized tools first.
The safe ASCII `basename` is at most 128 bytes. Optional image hints allow `avif`,
`gif`, `heic`, `jpeg`, `png`, `tiff` and `webp`; they do not verify media. SVG and other files
can be sent with the hint omitted. Preview and recipient viewing are not assured.
Artifact lookup grants no egress permission and does not renew retention.

Only one binary operation runs at a time. After verifying all originals, the
bridge stages them in sequence, then completes the group with the frozen comment.
Retained originals total at most 16 MiB; the last read can temporarily add another
bounded original before the aggregate check. Slack's external upload flow is bounded
and not automatically retried. Upload tickets must use the locally supported
HTTPS `files.slack.com:443/upload/v1/…` shape without redirects, credentials,
query or fragment; unsupported provider hosts fail closed. Bot tokens go only
to the configured Slack API, never to upload tickets.

Results distinguish `shared`, `not_shared`, and `share_unknown`, with closed
`stage`/`reason` fields, `authority_retired`, indexed `attachments` statuses, and
`text_status` (the group status or `not_requested` for no composed comment).
Staging failure leaves all items `not_shared`. Malformed, partial or uncertain
completion makes every item `share_unknown`: some or all may already be shared.
There is no atomicity or rollback guarantee. An interrupted upload may leave
private data on Slack. Never automatically retry an uncertain completion.
Same-call replay returns the retained outcome without another upload,
but new call IDs or restart can duplicate an uncertain share. A successful file share
returns no `message_ref` and creates no sent-message fact, edit or reaction
authority because Slack does not guarantee a native message timestamp.
No live configuration, credential or installation change is automatic.

### Incoming original files

Grant `slack:attachments:read` or the exact scoped `slack_list_attachments` and
`slack_download_attachment` tools, or enable the whole Slack tool group.
Add bot OAuth `files:read` and reinstall/re-authorize separately; retain current
message-event/history and identity scopes. No new file-event subscription is
required. Source support is not proof of live grants.

List with `message_ref`, using a delivered reference or `slack_get_message`'s
`message_id`. Download with that same `message_ref` and a listed `attachment_ref`.
Neither tool accepts native file IDs, URLs or paths. Historical reads use current
receive/author/thread policy, without gaining reply or mutation ownership.
Selections are same-agent and process/session-scoped, expire after 15 minutes,
and retire on relisting, observed mutation, failed refresh or lifecycle loss.
List again with the durable message ID after expiry or restart.

Download checks fresh source membership and current metadata before transfer,
after transfer and after artifact import. It imports the original bytes and
returns `key`, canonical `artifact`, `size`, `name` and an unverified `mime_type`
hint. Use separately authorized artifact import or image inspection tools to
consume the original. Content stays untrusted.
Artifacts persist independently of ephemeral transcripts; retirement may leave
undisclosed imported bytes.

Limits are 16 files per message, 16 MiB per original and one binary operation
shared with outbound uploads. Only hosted originals at checked HTTPS
`files.slack.com:443/files-pri/…` URLs are supported, without redirects, URL
credentials, query or fragment. Remote/external files, unsupported hosts/types
and unresolved Slack Connect metadata fail closed. This local support policy
does not cover every Slack deployment. Never substitute arbitrary URL fetching.
File-only presence follows normal sender/route policy; it cannot satisfy
`mentions_only`. A notice or listed name does not guarantee readable bytes.

### Linked messages

`message_ref` identifies the message itself; optional typed `root_id` identifies
the actual thread root, never an immediate parent. Live ingress does not query
history or guess `previous_id` from arrival order. `reply_to_id` is absent because
Slack supplies no immediate-parent evidence. Edits supply fresh links.

The default-off `slack_get_message` (`slack:read`) accepts only `message_id` and
returns current `message_id`, `author`, `text`, and available root/previous links.
Use the message's own ID or a linked ID to recover permitted content, including
messages never delivered to Tau.
This is untrusted historical context, not a live request, activation, or write
authority. No search, history list, cursor or archive exists. Separate attachment
tools handle original reads. `attachment_count` is present only when the current
provider response supplies a files array; absence means unknown.
Whole-group role grants include this tool; exact grants for other tools do not.

Versioned `slack-message:v2:` IDs survive restart/cache eviction without new
persistence. Their encoded native coordinates are recoverable, not secret.
IDs are not capabilities: each call rechecks current registration, installation,
receive route, fixed-thread confinement, and strict/lax canonical sender admission
or the exact bridge bot's own identity. Dynamic DMs retain their exact allowlisted participant.
Proactive-only permission grants no history read; mention-only delivery triggers
do not restrict explicitly requested history. Old hashes and extreme-coordinate
fallback hashes need a surviving local target; old `slack-thread-root:` digests
never resolve. Current content can change or become unavailable.

Reads require the route's history scope (`channels:history`, `groups:history`,
`im:history`, or `mpim:history`) and `users:read`. Mention-only channel setups may
need an added scope and app reinstall; this change neither checks nor changes live
permissions.
At most eight workers run off the protocol reader. Each call makes one exact
fetch, at most one optional predecessor request, and at most one bot mapping
plus one account lookup for each checked author. Each request is capped at
30 seconds/64 KiB, without retry or pagination.

A complete ordered thread window covering the preceding 24 hours and at most
100 items can prove the immediate predecessor. Empty, busy, capped, malformed
or failed windows omit it. Unthreaded DMs may use the nearest earlier unthreaded
item; top-level channels have no inferred topic predecessor. A static participant
precheck suppresses disallowed predecessor IDs without skipping to earlier
permitted messages; retrieval still verifies the author live. Missing links mean
unknown, not that no predecessor exists. See
[linked context](../README.md#linked-message-context).

The 1,024-entry incoming native-owner index retains sources pinned by owned or
in-flight agent reactions. Evicting an unpinned source also removes its source
metadata, reaction selectors and deferred activation, without consuming pending reports.
If every native owner is pinned, a new canonical create installs no local
authority; it still confirms its report and releases its admission slot.

Use `slack_send` for posts and replies. `destination` is an alias
with `proactive_send: true`; optional `reply_to` and `thread_id` are mutually
exclusive message locators, not raw native IDs or timestamps. `reply_to` uses the
existing thread or starts a thread on the selected top-level message. `thread_id`
requires a root locator and a destination, not a child or nested thread.
Selectors must match the current installation/conversation and any configured
fixed root. Destination-only sends post top-level or to the configured fixed root.
Reply-only shorthand resolves to a configured outbound grant, preferring an exact
fixed-thread route before a whole-conversation route.

Versioned `slack-message:v2:` locators survive restart and eviction. They grant no
permission; ordinary configured outbound authority does. No live ingress record,
registration, same-agent source ownership, canonical confirmation, or historical
read is required for sending. Legacy hashes need surviving address metadata.
Receive-only routes and dynamic-DM links no longer authorize sends.
Migration requires an operator to enable outbound authority on the intended
static route; never modify grants automatically. Top-level replies now start a
thread; previously, they posted top-level. Do not automatically substitute a
top-level post after any rejected or uncertain reply.

Replies and proactive sends use the agent-supplied message unchanged. Message
limits, the text-only retry budget, ordinary outbound authority, routing, threads,
authorization, and configuration freeze remain independent of its contents.
Ordinary send permission authorizes native mentions in `message` for replies,
proactive posts and attachment comments: `<@U123>` or W account,
`<!subteam^S123>` user group, and `<!here>`, `<!channel>`, `<!everyone>`.
`<#C123>` is a channel link, not a mention. No extra grant, human-only restriction,
recipient allowlist or fresh directory/membership check applies.
Plain `@name` is not resolved; automatic name expansion remains disabled.
Optional `mention_users` accepts 1–32 exact U/W IDs or current retained same-agent
`slack-sender:…` account references (1–128 bytes each) and prepends native account
mentions. Bot accounts are allowed; B integrations are not account selectors.
The bridge rejects unavailable, stale or other-agent references with categorical
errors and no network I/O.
This works with either route and attachments but grants no send or route authority;
native mentions in `message` need no structured field. Generated mentions stay
wire-only; authored native text stays in the durable logical message. The bridge
rejects final composed text above 40,000 Unicode scalars rather than truncating it.
Slack acceptance does not guarantee rendering, notification, delivery or reading.
Bridge help/control/error output is escaped,
bounded, and sent with mrkdwn/link expansion disabled.
For source replies only, optional `mention_source_user: true` prepends a mention
of the exact verified human already bound to `reply_to`; it defaults false,
accepts no identity argument, works with a matching `destination`, and is frozen
into the exact body, including any text-only retry. This optional convenience
still needs retained same-agent verified human metadata. If unavailable, omit
it or use `mention_users`; it is not a prerequisite for an ordinary reply.

`slack_conversations` is disabled by default and separately authorizable through
its exact prefixed name or `slack:discover` tag. It returns all static routes,
including receive-only records, in bounded sorted pages with alias, kind, scope,
optional description, and factual receive/proactive policy. It is a local
informational read: no worker startup, registration, authority grant, or config
freeze. It excludes native ids/roots, dynamic links, identities, registrations,
selections, runtime state, and Slack-fetched metadata. Group-enabled roles gain
this inventory surface; use exact policy or separate prefixed instances to isolate
it. Send resolves the current alias without a snapshot token, so same-alias reuse
is operator responsibility.

Every role granted one instance's send tool can use all its proactive aliases.
Untrusted receive content can influence such a role, so keep destination sets and
roles minimal; use separate roles or prefixed extension instances for isolation.

Configuration freezes at automatic Socket Mode launch, after successful manual
Socket Mode preflight, or immediately
before an authorized post or reaction API attempt; restart Tau to change it. Failed preflight and denied
sends remain reconfigurable. A bounded process-local native-id cache replays exact
pending reports and drops repeats after canonical confirmation; cache eviction or
restart may duplicate delivery. Recording precedes identity and report
construction, so an earlier failure remains suppressed until eviction/restart.
Runtime
links/routes/selections and edit ownership clear on restart. Logs and notices
omit payloads, ids, websocket URLs, and tokens.

Socket Mode admission starts fail-closed in every extension process. An
extension restarted after session initialization becomes active only after the
harness supplies its replay-marked current `session.started` snapshot; an
extension present during initialization receives the start live instead.
Catch-up restores only current-session admission. It does not reconstruct
process-local message, reply, edit, reaction, registration, selection, or
deduplication authority.

`slack_send` reserves each accepted `ToolCallId` in a non-evicting 1,024-entry
process/session ledger and moves authentication and delivery work off the
serialized protocol reader. Without installation evidence, it
retains the original call in separate bounded preparation until `auth.test`
finishes; no registration or caller retry is needed. Same-call replay coalesces,
and conflicting reuse before post reservation cancels the pending call with one
stable conflict error rather than leaving its original send running.
Configuration/session/agent retirement invalidates the original call rather
than rebinding it. A started auth request retains its worker slot until it
returns, then cancels stale work without installing identity or posting.
Preparation and delivery share the 1,024-call replay capacity and 64-worker
budget; excess new calls fail before I/O. For text-only sends, it makes one
initial post attempt and at most one byte-identical retry after bounded
Retry-After or an ambiguous outcome. This is
at-least-once notification delivery: an ambiguous first attempt followed by
success may leave one or two Slack copies; two ambiguous outcomes can leave
zero, one, or two. Successful retry results report
`delivery_copies: one_or_two_possible`. After text-post success Slack writes
transient `message.sent_reported` and then transient `tool.result_reported`
observations through one serialized local write-and-flush gate; the harness later
derives canonical facts. The configured publisher's matching canonical
`message.sent` live echo completes the pending ledger and installs local
message/reaction authority; the typed result remains separate.
Same-id/same-argument replay coalesces before that echo, then returns only the
stable result without reposting or republishing; conflicting reuse errors and a
new call id is new intent. Unregister, unload, route/config/session changes, and shutdown cancel
retry and stale publication authority. The ledger clears on
session/process retirement, so there is no durable outbox, `client_msg_id`,
restart guarantee, or exactly-once claim. Provider diagnostics are closed
categories; raw bodies, Slack error text/headers, native ids, mentions, tokens,
and message content never appear in model errors, notices, or logs.
Each HTTP attempt has a 30-second timeout and each response is capped at
64 KiB. The post retry must begin within 60 seconds of authenticated preparation;
cold authentication does not consume that existing retry horizon.

Supported events reserve one of 64 process-local queued/in-flight admission slots
before ACK. Slow live sender verification and bridge-local replies run on one serial
worker, so they do not block later websocket ACKs, Ping/Pong, reconnect, or
shutdown. Report-bearing work retains its slot through canonical confirmation;
missing echoes saturate admission and reconnect without ACK so Slack can retry.
Socket Mode ACK remains separate from Tau commit. The FIFO survives
reconnect but is memory-only; process death after ACK can still lose an occurrence.
DNS, TCP, TLS, and WebSocket upgrade establishment share a 30-second deadline,
remain preemptible by shutdown, and time out through the existing bounded
connection-failure notice and reconnect backoff. DNS retains OS/NSS results and
ordering. An executing OS lookup cannot be cancelled, so it retains the sole
process-wide resolver permit after caller timeout or shutdown; retries and
concurrent extension instances cannot accumulate resolver calls. This bounds the
worker, not runtime destruction if the OS lookup never returns. The worker sends its first
WebSocket Ping after 10 seconds and repeats every 10
seconds. An independent deadline reconnects 40 seconds after the latest Pong;
other traffic does not refresh liveness, and blocked Ping/Pong/ACK writes remain
preemptible by shutdown and the deadline. This prevents a half-open connection
from silently disabling ingress indefinitely. The first startup or reconnect
failure emits one bounded warning per process. Tungstenite caps each Socket Mode
frame and complete text or binary message at 256 KiB before decoding; equality is
accepted, while the first excess byte drops the socket and uses the same bounded
reconnect path.
For latency troubleshooting, enable `TRACE` and inspect `slack_latency_v1`
monotonic stage markers. They contain bounded classes and process-local ordinals,
not Slack identifiers, message text, tokens, URLs, agent identities, or durable
events; retain them only in bounded local logs.

## Send permission diagnosis

The bridge retains only a closed set of diagnostic labels for send permission
errors. Cold-send authentication denial includes `stage=auth.test`; text-post
denial includes `stage=chat.postMessage`. The reason is one of `http_403`,
`not_in_channel`, `restricted_action`, or `missing_permission`. `http_403`
identifies only the HTTP rejection: the bridge does not inspect the response body
or infer a Slack API reason. The other labels preserve an allowlisted provider
category without identifying the underlying policy cause. Missing scope remains
a separate error.

For each cold-send provider authentication failure, the extension includes
`stage=auth.test` in the agent's tool error. It also emits WARN tracing events for
failed cold-send authentication and each failed text-post attempt. Post warnings
include `stage=chat.postMessage`, the closed category and reason, the send class
(`proactive` or `agent_reply`), and the attempt disposition (`definitive_failure`,
`outcome_unknown`, or `rate_limited`). Authentication warnings include the fixed
stage and closed typed error. You do not need TRACE latency logging to see these warnings.
They contain no destination, agent identity, message body, token, URL, or raw
provider text. After an attempt warning, an existing bounded retry may still
succeed, and an uncertain attempt may have posted. The warning is not a final
delivery result.
Same-call terminal replay does not repeat provider attempts or their warnings.
The extension does not invent a numeric HTTP status from a provider error code.

The older plain `Slack permission was denied` result does not identify the stage
or reason. Before interpreting diagnostics, check already-retained tool results
and the deployed build identity; a current tool schema alone does not establish
build identity. Conversation discovery describes configured routes, not caller
grants, Slack membership, or provider readiness. These diagnostics do not
authorize retries or changes to configuration or grants. They also do not prove
that changing mentions fixes a denial. Preserve any remote-copy uncertainty
reported with the error.

## Socket Mode reconnection diagnosis

For the next harness start, use:

```sh
TAU_LOG='slack=trace,warn' tau
```

`slack` is the extension's exact tracing target. Set the variable when starting
or restarting the harness; setting it on a later `tau attach` changes only that
new UI process, not the already-running extension.

First find the affected session with `tau session list`, then set its log
directory. The authoritative private stderr file is
`logs/<configured-extension-instance>.log`, so list it rather than assuming the
instance is named `std-slack`:

```sh
logs="${XDG_STATE_HOME:-$HOME/.local/state}/tau/sessions/<session_id>/logs"
find "$logs" -maxdepth 1 -type f -name '*.log' -printf '%f\n'
grep -lE 'target=slack|Slack Socket Mode' "$logs"/*.log
```

The Slack-specific fields are `lifecycle=connected|hello|reconnecting|degraded|stopped`,
`ack=sent|failed`, `failure=socket_worker|installation_restart_required`,
`failure_class=...`, `reconnect_reason=...`, `stop_reason=...`, and
`rejection=...`. The closed `failure_class` and reason fields identify the
worker stage without raw provider or transport detail. The README explains their
connection, heartbeat, and installation meaning. For the standard process, log,
mirror, privacy, and retention workflow, use
`tau-self-knowledge-debugging-extensions`.

These are deliberately closed-category logs: Slack-owned records omit tokens,
websocket URLs, native IDs, message payloads, and provider response text. The
raw per-extension stderr file is nevertheless private and unredacted at its sink
boundary; dependency or custom-extension output can contain identifiers or local
paths. Review it before sharing. See the [README troubleshooting
procedure](../README.md#troubleshooting) for the detailed
reconnection runbook.

For history and exact-message reads, the bridge reports retrieval, provider and
policy diagnostics with bounded `stage` and `reason` labels and a next action.
Schema validation, capacity, call-ID conflict, spawn and panic errors retain
plain informative strings. History distinguishes preparation, history fetch,
page validation, author verification and final disclosure. Exact reads use
a generic message-fetch stage even for author lookup, so errors do not reveal
whether the target exists or reached verification. `unavailable_or_policy`
does not diagnose a permission problem. History filters messages from verified
but disallowed authors rather than reporting an identity-verification failure.

`missing_scope` means the provider reported that category at the named stage;
ask the operator to compare installed grants with documented requirements.
Transport, malformed-page and unverified-identity errors do not prove a scope
problem. Do not broaden scopes or reinstall without evidence. The shared bounded
read helper distinguishes an observed body limit (`response_too_large`) from
an unsuccessful body read (`response_read_failed`), without raw HTTP detail.
After the original authority or history scheduling budget expires, the bridge
replaces any pending provider diagnosis with `lifecycle_changed` or
`deadline_exceeded`. These errors reveal no stale provider facts. Capture the
categorical error and call timing for debugging; do not share tokens, raw responses,
private message text or native identifiers, and do not scan or retry blindly.

For missing delivery, check the exact `message.*`/`app_mention` subscription and
history/app-mention scope, reinstall after scope changes, verify app membership,
and ensure app/bot tokens share one workspace installation. `missing_scope`
identifies the required scope. `channel_id_changed` makes an exact route stale;
update it and restart. Missing edits/reactions additionally require the broad
message event or both reaction events plus `reactions:read`.

## Agent reactions

Enable the whole Slack group, or grant the exact scoped `slack_react` tool or
`slack:react` tag. The tool remains disabled by default.
It accepts `{message_ref, emoji, action: add|remove}` only for exact Tau-issued
refs from canonically confirmed incoming create/edit facts or successful text-only
`slack_send`. Refs use versioned message locators or legacy hashes, but it accepts
no channel IDs or timestamps as separate route selectors, aliases, Unicode
emoji, list, toggle, or discovery.
Removal is limited to same-agent reactions unambiguously added in the current
runtime. Add `reactions:write`, reinstall the app, and keep the bot a member of
target conversations.
Slack success commits local add/remove ownership only after the successful tool
result is written and flushed locally. Writer failure retires the whole Slack
session without retrying or compensating the remote reaction; local flush is
not a harness commit acknowledgement.
The extension retains 256 reaction call attempts without eviction until agent or
session lifecycle cleanup. When this ledger is full, new call IDs fail with
`Slack reaction attempt capacity is full` before Slack I/O; exact retained
replays still return or coalesce their original disposition without reposting.
The CLI renders inbound reaction action, the bounded configured alias/display
when available (otherwise the opaque actor reference), and the exact
custom/skin-tone name; it performs no Unicode emoji lookup.

Successful text-only `slack_send` results use
`{"status":"sent","message_ref":"slack-message:v2:<encoded>","delivery_copies":"one"|"one_or_two_possible"}`
(replacing the former plain-text success). The ref remains inert after the local
sent-report/result write and flush; only the matching canonical `message.sent` downpath echo
activates it.
