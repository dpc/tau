---
name: tau-self-knowledge-ext-zulip
description: Use for Tau std-zulip setup, event queues, stream/topic and DM routing, tools, security, mutations, or troubleshooting.
---

# Tau std-zulip extension self-knowledge

`std-zulip` is Tau's disabled-by-default first-party Zulip bridge. It uses bot email/API-key HTTP Basic authentication, `POST /api/v1/register`, and long-poll `GET /api/v1/events`; it does not use webhooks.

Configure `site`, `bot_email_secret`, `api_key_secret`, a stable `identity_key_secret`, a nonempty numeric `allowed_user_ids`, optional sender aliases, optional `direct_messages: { receive: all_messages }`, and exact stream/topic routes. Keep the identity key stable across API-key rotation; changing it deliberately starts a new opaque sender/conversation/message namespace. Routes independently select `receive: mentions_only|all_messages` and `proactive_send`. Production requires HTTPS.

The disabled tools are `zulip_register`, `zulip_conversations`, `zulip_send`, and separately tagged `zulip_react`; `tool_prefix` scopes all names and the group. Replies and reactions require opaque Tau-issued live references. Proactive sends require configured aliases. Native stream, participant, message, queue, and credential values never become model authority.

The extension emits generic message reports for creates, edits, deletes, reactions, and successful sends. Queue loss reconnects live and warns about a possible gap; it does not fetch missed backlog. Runtime references, dedupe, registrations, and cursors disappear on restart. The bridge is Markdown text-only and deliberately provides no file upload/download capability.

See `crates/tau-ext-zulip/README.md`, `SECURITY.md`, `testing.md`, and local specs for complete behavior.
