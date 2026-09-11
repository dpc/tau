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
`GET /api/v1/events`; it does not use webhooks. The current executable requires
Tau protocol 6.0 and registry SDK 0.3.0. Its configuration schema, snake_case
keys, secret bindings, and catch-up checkpoint format remain unchanged.

Configure `site`, `bot_email_secret`, `api_key_secret`, a stable `identity_key_secret`, a nonempty numeric `allowed_user_ids`, optional sender aliases, optional `direct_messages: { receive: all_messages }`, optional `proactive_direct_messages` aliases with one fixed recipient each, and name-based stream/topic routes. Keep the identity key stable across API-key rotation; changing it deliberately starts a new opaque sender/conversation/message namespace. `allowed_user_ids` admits inbound senders only; it does not authorize proactive DMs. Routes independently select `receive: mentions_only|all_messages` and `proactive_send`; every configured channel name resolves to a private native ID before queue registration, and `all_messages` subscribes the bot idempotently before that registration without later unsubscribing. Exact proactive stream names remain the default, while `agent_chosen_topic: true` on a proactive name without `topic` explicitly grants agent topic choice within that configured channel. Production requires HTTPS.

Set `non_allowlisted_activity: {}` to collect bounded stream activity that
passes every receive predicate except the numeric sender allowlist.
Unauthorized message bodies are discarded. The next allowlisted message in
the same exact stream/topic may prepend one bridge-authored note with sanitized
untrusted display hints, route-scoped opaque pseudonyms, and post counts; its
own Markdown remains the exact suffix, and the pair uses one fact and wake.
This is best effort, not a reliable queue: bounded process state can expire or
disappear after 24 hours, authority changes, or restart. Capacity can omit new
activity, duplicate-cache eviction can permit duplicate observations, and
nothing is delivered without a later eligible message. Direct messages and
autonomous deadline delivery are not supported.

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
actor require a top-level numeric allowlisted actor. Zulip's singular delete may
omit its actor only when its top-level message ID, message type, and stream/topic
fields match one exact current source owner; the checked report records no actor
and successful publication revokes only that owner. Bulk, nested-ID,
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

Restoration resumes prior explicit intent rather than synthesizing a model tool
call or deriving authority from historical roles or UI summaries. Only paired
accepted starts and recognized versioned effective terminal metadata establish
intent; old metadata-free results, unfinished or unrecognized outcomes,
incomplete replay, and bounded correlation exhaustion do not. The bridge gives
no fallback prompt or notification and does not retry failed restoration in the
same load; an explicit enable remains available. It restores no old queue,
native route, source reply reference, or reaction ownership. Catch-up remains
independently opt-in and keeps its existing checkpoint; send-only mode never
restores receive registration.

Queue-poll failures retain content-free classifications for bounded body-read,
JSON, result-envelope, queue, and events-shape failures. Startup and
re-registration failures retain only the operation, HTTP status, and a bounded
uppercase Zulip machine code. These diagnostics expose no response bodies,
credentials, queue IDs, native IDs, routes, or message content. They classify
future failures but do not prove the exact subtype or root cause of any past
live incident.

The separately maintained `tau-ext-zulip` project owns the complete operational,
security, testing, architecture, and routing documentation.
