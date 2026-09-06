---
name: tau-self-knowledge-ext-zulip
description: Use for Tau std-zulip setup, event queues, stream/topic and DM routing, tools, security, mutations, or troubleshooting.
---

# Tau std-zulip extension self-knowledge

`std-zulip` is Tau's disabled-by-default configuration for the separately
maintained `tau-ext-zulip` executable. Tau does not bundle or install that
executable; install it separately and ensure `tau-ext-zulip` is available
through `PATH` before enabling the instance. Tau still starts it through the
normal supervised stdio extension route. The bridge uses bot email/API-key HTTP
Basic authentication, `POST /api/v1/register`, and long-poll
`GET /api/v1/events`; it does not use webhooks.

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

In ordinary mode, the disabled tools are `zulip_register`, `zulip_conversations`, `zulip_send`, and separately tagged `zulip_react`; `tool_prefix` scopes all names and the group. Replies and reactions require opaque Tau-issued live references. Proactive sends require configured destinations; `zulip_send` accepts `topic` only for a discovered stream name explicitly marked `agent_chosen_topic`, and `topic: ""` is Zulip general chat. A proactive-DM alias sends only to its one configured recipient; callers cannot supply user IDs. Native stream, participant, message, queue, and credential values never become model authority.

The extension emits generic message reports for creates, edits, deletes,
reactions, and successful sends. `offline_message_catch_up` defaults to false,
preserving live-only reconnect behavior. When enabled, it registers a fresh live
queue, retrieves bounded created-message history after an identity-scoped
durable checkpoint, merges/deduplicates the live overlap, and advances only
after its canonical delivered fact returns on the post-persistence downpath.
First use establishes the current baseline without replay. Offline edits,
deletes, and reactions are not recovered; filter changes do not rescan before
the checkpoint. Crash recovery is at-least-once and can duplicate messages.
Runtime references and registrations still disappear on restart. The bridge is
Markdown text-only and deliberately provides no file upload/download capability.
Admitted Zulip Markdown remains exact through canonical facts, replay, and
provider context, including a leading addressed bot mention.

When Zulip rejects the initial or live-re-registration `users_me`,
`get_stream_id`, `subscribe`, or `register` request, the diagnostic keeps its
authentication, rate-limit, invalid-request,
or unavailable category and adds only the operation, HTTP status, and a
1–64-byte uppercase ASCII `[A-Z0-9_]` machine error code. Missing, malformed,
or oversized codes show as `unknown`; no response message/body, request data,
headers, URL data, or credentials appear.

The separately maintained `tau-ext-zulip` project owns the complete operational,
security, testing, architecture, and routing documentation.
