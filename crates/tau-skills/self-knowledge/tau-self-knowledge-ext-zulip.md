---
name: tau-self-knowledge-ext-zulip
description: Use for Tau std-zulip setup, event queues, stream/topic and DM routing, tools, security, mutations, or troubleshooting.
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
`GET /api/v1/events`; it does not use webhooks. The sender-trust-capable
extension revision requires Tau protocol 7.0 and registry SDK 0.4.0; configured
6.x bridges are rejected before Configure/Ready. Its snake_case keys, secret
bindings, and catch-up checkpoint format otherwise remain unchanged.

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

For one fixed outbound DM with no Zulip ingress, set `send_only: true`, omit all inbound fields, and configure exactly one `proactive_direct_messages` alias. This mode declares only scoped `zulip_send` without a tool group; sending uses `message` plus that sole alias and needs no registration. It never registers or polls a queue, publishes Zulip-originated events, installs reply/reaction authority, or activates an agent. Mode changes require extension restart.

In ordinary mode, the disabled tools are `zulip_register`, `zulip_conversations`, `zulip_send`, and separately tagged `zulip_react`; `tool_prefix` scopes all names and the text tool group. Replies and reactions require opaque Tau-issued live references. Proactive sends require configured destinations; `zulip_send` accepts `topic` only for a discovered stream name explicitly marked `agent_chosen_topic`, and `topic: ""` is Zulip general chat. A proactive-DM alias sends only to its one configured recipient; callers cannot supply user IDs. Native stream, participant, message, queue, and credential values never become model authority.

The approved attachment-capable extension revision adds
`zulip_send_attachment`. An `std-zulip` instance exposes it only when its
installed `tau-ext-zulip` implements that revision's attachment contract. An
ordinary-mode role can share one existing shared Artifact file or image only
when it explicitly grants that exact scoped tool. `zulip_send_attachment` is
disabled by default, tagged `zulip:attach`, and has no tool group, so text-send
permission does not authorize uploads. Register first, then provide a canonical
shared `blake3:<64 lowercase hex>` Artifact `key`, a safe ASCII basename
`filename`, optional Markdown `message` caption, and exactly one existing
`destination` or `reply_to`. Filenames are 1–128 bytes, cannot start with `.`,
and contain only letters, digits, `.`, `_`, and `-`. `topic` follows the same
explicit agent-chosen-topic authority as text sends.

That revision also accepts optional `content_type`. When supplied, it must be
exactly one of `image/avif`, `image/gif`, `image/heic`, `image/jpeg`,
`image/png`, `image/tiff`, or `image/webp`, and becomes the multipart file-part
`Content-Type`. When it is omitted, the file part remains
`application/octet-stream`. It is an unverified caller declaration: the
extension neither infers it from the filename, Artifact metadata, or artifact
bytes, nor validates the bytes against it. A non-string or unsupported value
fails before Artifact RPC or provider effects. Use `image/png` for a generated
PNG. The outbound Markdown remains the same `[filename](/user_uploads/...)`
link. A supported image MIME can let Zulip, its server, or clients process a
preview, but does not force inline display.

The tool verifies at most 16 MiB of original binary bytes through Artifact
RPC, uploads once, then sends to the frozen route. It accepts files as well as
images; Zulip selects previews and can impose a lower file-size limit. It never
accepts local paths, raw bytes, remote URLs, inbound downloads, public temporary
links, or harness-store access. The sent fact preserves the exact outbound
Markdown, including an ordinary authenticated relative upload link as inert
content, never as recipient or reply authority.

One attachment runs at a time. Artifact reads have one 60-second total
deadline, while provider operations retain 30-second HTTP deadlines. Sharing
does not renew Artifact retention: existing originals persist independently
of ephemeral transcripts and can become unavailable. Upload and send are
separate effects, so failure or cancellation can leave an orphan upload or an
uncertain sent message. Do not retry an uncertain send automatically; inspect
the conversation first. Send-only mode remains text-only.

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
references disappear on restart. The bridge does not download inbound files.
Admitted Zulip Markdown remains exact through canonical facts, replay, and
provider context, including a leading addressed bot mention.

A successfully completed ordinary-mode `zulip_register {"enabled":true}`
records explicit receive resume-intent. After complete successful session and
agent replay, a runtime restart or agent reload can establish a fresh
registration under the current configuration, routes, allowlists, admission
rules, and loaded membership. Unloaded agents receive nothing. Explicit
disable, or an enable attempt that actually retires registration and then
fails, revokes intent; rejection before retirement leaves it unchanged. A live
configuration change retires runtime authority and defeats pending restoration;
a later reload may resume retained intent under that new configuration.

An ordinary receive-enabled startup subscribes both historically and live to
the seven durable facts that reconstruct this intent: `tool.started`,
`provider.tool_result`, `provider.tool_error`, `tool.background_result`,
`tool.background_error`, `tool.cancelled`, and `session.agent_loaded`.
`agent.replay_complete` and `session.replay_complete` remain live-only
boundaries, so restoration waits for current replay completion. Send-only
startup remains live-only.

Restoration resumes prior explicit intent rather than synthesizing a model tool
call or deriving authority from historical roles or UI summaries. Only paired
accepted starts and recognized versioned effective terminal metadata establish
intent; old metadata-free results, unfinished or unrecognized outcomes,
incomplete replay, and bounded correlation exhaustion do not. The bridge gives
no fallback prompt or notification and does not retry failed restoration in the
same load; an explicit enable remains available. It restores no old queue,
native route, source reply reference, or reaction ownership. Catch-up remains
independently opt-in and keeps its existing checkpoint; send-only mode never
restores receive registration. Historical intent replay does not fetch old
Zulip messages; only `offline_message_catch_up` can request bounded
created-message history.

## Diagnose receive restoration and queue recovery

Zulip writes content-free diagnostics to its supervised extension stderr, which
Tau captures in the extension log. By default, `zulip=info,warn` makes the
extension's `info` and higher records visible, and leaves unrelated targets at
`warn` and higher. `TAU_LOG` can override that filter; set it in the environment
that starts the harness, then restart the harness or session so its new
extension child inherits it, as described in
`tau-self-knowledge-debugging-extensions`.

Replay-boundary records include an `ok` or `error` outcome and bounded
loaded/replay-complete/durable-intent/eligible flags or counts. Receive
restoration records show candidate selection, attempt, success, or a stable
failure category: `superseded`, `not_configured`, `send_only`, `resolve_stream`,
`validate_routes`, `subscribe`, `register_queue`, `checkpoint_config`,
`checkpoint_open`, `authority_changed`, `worker_start`, or
`restore_worker_spawn`. Queue diagnostics likewise report invalidation, its
recovery mode, setup-stage failures, and successful recovery. Existing malformed
event-batch and long-poll warnings remain available.

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
