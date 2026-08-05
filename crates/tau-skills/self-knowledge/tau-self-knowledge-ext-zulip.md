---
name: tau-self-knowledge-ext-zulip
description: Use for Tau std-zulip setup, event queues, stream/topic and DM routing, tools, security, mutations, or troubleshooting.
---

# Tau std-zulip extension self-knowledge

`std-zulip` is Tau's disabled-by-default first-party Zulip bridge. It uses bot email/API-key HTTP Basic authentication, `POST /api/v1/register`, and long-poll `GET /api/v1/events`; it does not use webhooks.

Configure `site`, `bot_email_secret`, `api_key_secret`, a stable `identity_key_secret`, a nonempty numeric `allowed_user_ids`, optional sender aliases, optional `direct_messages: { receive: all_messages }`, optional `proactive_direct_messages` aliases with one fixed recipient each, and exact stream/topic routes. Keep the identity key stable across API-key rotation; changing it deliberately starts a new opaque sender/conversation/message namespace. `allowed_user_ids` admits inbound senders only; it does not authorize proactive DMs. Routes independently select `receive: mentions_only|all_messages` and `proactive_send`; exact proactive stream aliases remain the default, while `agent_chosen_topic: true` on a proactive alias without `topic` explicitly grants agent topic choice within that configured stream. Production requires HTTPS.

The disabled tools are `zulip_register`, `zulip_conversations`, `zulip_send`, and separately tagged `zulip_react`; `tool_prefix` scopes all names and the group. Replies and reactions require opaque Tau-issued live references. Proactive sends require configured aliases; `zulip_send` accepts `topic` only for a discovered stream alias explicitly marked `agent_chosen_topic`, and `topic: ""` is Zulip general chat. A proactive-DM alias sends only to its one configured recipient; callers cannot supply user IDs. Native stream, participant, message, queue, and credential values never become model authority.

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

When Zulip rejects the initial or live-re-registration `users_me` or `register`
request, the diagnostic keeps its authentication, rate-limit, invalid-request,
or unavailable category and adds only the operation, HTTP status, and a
1–64-byte uppercase ASCII `[A-Z0-9_]` machine error code. Missing, malformed,
or oversized codes show as `unknown`; no response message/body, request data,
headers, URL data, or credentials appear.

See `crates/tau-ext-zulip/README.md`, `SECURITY.md`, `testing.md`, and local specs for complete behavior.
