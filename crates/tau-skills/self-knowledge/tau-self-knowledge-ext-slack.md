---
name: tau-self-knowledge-ext-slack
description: Use this extension skill when the user asks about Tau's std-slack extension, Slack Socket Mode setup, scopes, event subscriptions, slack_register/slack_send, routing, security modes, edits, reactions, or troubleshooting Slack delivery.
advertise: false
---

# Tau std-slack extension self-knowledge

`std-slack` is Tau's disabled-by-default Slack Socket Mode bridge. It exposes
`slack_register` and `slack_send`; agents never choose arbitrary Slack
destinations. Incoming Slack text is always untrusted external content.

## Configuration

Configure `extensions.std-slack` with an app-level `xapp-...` secret carrying
`connections:write`, a workspace-installation `xoxb-...` bot secret,
`allowed_user_ids`, and either configured `channel_ids` or the single linked-DM
mode. `security_mode` defaults to `strict`; `listening_scope` defaults to
`mentions_only`. Configuration is immutable after the worker starts, so restart
Tau to apply changes.

`strict` admits only allowlisted Slack-verified humans. `lax` also admits
verified humans in already authorized conversations, but never grants them DM
linking, agent selection, bridge commands, or destination control.
`mentions_only` requires `app_mention` in configured channels.
`all_messages` additionally accepts eligible ordinary channel messages.

## Slack app scopes and events

Every setup needs app-token scope `connections:write`, bot-token scope
`users:read` for all ingress identity verification, and `chat:write` for Slack
replies and command responses. Selected behavior rows are additive:

| Behavior | Additional bot events | Additional bot scopes |
| --- | --- | --- |
| Channel mentions | `app_mention` | `app_mentions:read` |
| Channel edits and `all_messages` | `message.channels` | `channels:history` |
| Linked DMs | `message.im` | `im:history` |
| Reactions to Tau posts | `reaction_added`, `reaction_removed` | `reactions:read` |

Private channels use `message.groups` plus `groups:history`; MPIMs use
`message.mpim` plus `mpim:history`. Both must be explicitly listed in
`channel_ids`; the linked-DM mode accepts only a one-to-one IM. After changing
scopes or subscriptions,
reinstall the app to the workspace, update the bot secret if Slack refreshes
the token, invite the app to configured conversations, and restart Tau. Edits
are `message_changed` events delivered under
the relevant `message.*` subscription, not `app_mention`.

Signing Secret, Client ID, Client Secret, Slack App ID configuration, Incoming
Webhooks, Slash Commands, OAuth redirect URLs, `channels:read`, `groups:read`,
`users:read.email`, `reactions:write`, `chat:write.public`, and file scopes are
not required.

## Routing and lifecycle

An agent first calls `slack_register(enabled: true)`. A top-level Slack message
gets a top-level reply; a threaded message stays in its source thread.
`slack_send` requires the opaque canonical `reply_to` from the incoming
envelope. Reactions route only when they target a recent message successfully
posted by `slack_send`; arbitrary Slack posts are ignored. Edits route only for
recent committed incoming messages with matching sender, conversation, thread,
and native identity.

Socket Mode acks before routing. Expected policy rejections remain quiet;
operator logs expose identifier-free connect, hello, ACK, degraded, and
reconnect states. Tokens, websocket URLs, envelope IDs, and payloads are not
logged.

## Troubleshooting

- `users.info user_not_found` with a user that succeeds manually can indicate
  an old Tau build that sent JSON instead of form encoding; current Tau uses a
  form-encoded request. Rebuild/restart first.
- `missing_scope` means add the named bot scope, reinstall, refresh the secret,
  and restart. `app_mention` can work while edits or reactions remain absent.
- No edit: verify the matching `message.*` event and history scope.
- No reaction: verify both reaction event subscriptions and `reactions:read`,
  and react only to a post created by the current Tau session through
  `slack_send`.
- If Socket Mode works but identity calls fail, ensure the app and bot tokens
  belong to the same app installation/workspace.
