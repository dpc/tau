# DESIGN-tau-ext-slack-safe-source-mentions: Constrain Slack source mentions

Status: confirmed, 2026-07-15, dpc

**Related:** [DESIGN-tau-ext-slack-sender-identity](DESIGN-tau-ext-slack-sender-identity.md),
[DESIGN-tau-ext-slack-canonical-reply-selectors](DESIGN-tau-ext-slack-canonical-reply-selectors.md),
and [DESIGN-tau-ext-slack-send-delivery](DESIGN-tau-ext-slack-send-delivery.md)

## Decision

`slack_send` has one optional structured field:

```json
{
  "message": "I have a result.",
  "reply_to": "msg-…",
  "mention_source_user": true
}
```

Omitted and `false` are identical and produce no mention. `true` is valid only
with `reply_to`; it is invalid with proactive `destination`. The fixed schema
accepts no user ID, name, display, user alias, arbitrary mention target,
compatibility alias, or unknown field. There is no dedicated mention or user
discovery tool.

This is a source-only notification capability for roles already granted the
default-off `slack_send` tool. It uses the same `slack:send` tag and adds no
configuration switch or Slack scope. Selecting another eligible Tau-issued
`reply_to` can notify that occurrence's exact source, matching the role's
existing source-reply authority.

## Resolution and authority

For `mention_source_user: true`, the extension:

1. resolves the Tau-issued selector to the invoking agent's same-process,
   locally retained, live `ReplyRoute`;
2. revalidates current agent, session, extension lifecycle, configuration
   generation, exact conversation/thread, receive policy or dynamic-DM link,
   and exact installation team;
3. takes the target only from the route's ingress-verified U/W `user_id`; and
4. defensively rejects the active bot identity and `USLACKBOT`.

Display, aliases, message text, and destination configuration cannot select a
mention target. The route's ingress-time exact live-human `users.info`
attestation is authoritative; mention resolution performs no additional Slack
API call. Both allowlisted and lax-permitted verified-human static-route replies
are eligible. Dynamic DMs remain exact allowlisted-user links.

Create, edit, and human-reaction occurrences may establish eligible routes.
Rejected, inactive, replayed, orphaned, stale-installation, bot/app/self, and
nonhuman occurrences do not. Slack Connect uses the exact verified source U/W
identity and allows the actor's home team to differ from the already validated
installation team.

## Text and wire contract

Agent-authored message text must not contain raw Slack entity or notification
forms `<@`, `<!`, or `<#`. This rejection applies to reply and proactive sends.
Other mrkdwn remains supported. The extension always sends with:

- `mrkdwn: true`;
- `link_names: false`; and
- no `reply_broadcast`.

It constructs:

```text
presentation_text = (prefix_agent_id ? "[<agent-id>] " : "") + message
wire_text         = (mention_source_user ? "<@<private-source-id>> " : "") + presentation_text
```

The generated mention is always first and followed by one ASCII space. It enters
the typed final agent post only after model text has passed native-control
rejection, so model text cannot be interpreted as extension-generated control.
The post retains `mrkdwn:true` so Slack can interpret that private mention while
`link_names:false` prevents name-based expansion. The final decorated body must
be at most 40,000 Unicode scalar values; Tau fails rather than relying on Slack
truncation.

Logical durable outgoing text is `presentation_text`, not the generated
ID-bearing wire decoration. Tool invocation persistence records the safe boolean.
Tool results, progress, errors, canonical outgoing text, and operational logs do
not copy the native mention. Existing canonical endpoints may still contain the
already model-visible native stable sender ID; this feature creates no new
id-bearing protocol field.

## Delivery and failure behavior

The exact decorated `wire_text`, formatting flags, route, thread, expected bot,
and installation team freeze before remote I/O. The send pipeline performs one
initial `chat.postMessage` plus at most one byte-identical retry under
[DESIGN-tau-ext-slack-send-delivery](DESIGN-tau-ext-slack-send-delivery.md).
Retry does not re-resolve identity or rebuild text.

Deterministic validation fails before reservation or Slack I/O. Defensive
source/installation failures use one categorical error without identifiers.
Slack acceptance means only that it accepted the native mention post; Tau does
not claim delivery, reading, or an OS notification because Slack preferences,
DND, mutes, and administrative policy remain authoritative.

Thread mentions remain thread posts and never broadcast. Proactive sends have no
authenticated source and therefore cannot use this option.

## Excluded surface

This decision does not add proactive or third-party mentions, user aliases for
mentioning, directory enumeration, display-name resolution, `users.list`, user
groups, email/profile scopes, outbound editing, chunking, or a claim that native
Slack IDs are hidden from models. Those would require separate identity,
authorization, and privacy decisions.
