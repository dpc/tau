# SPEC-tau-ext-slack-source-mentions: Slack source mentions

`mention_source_user` defaults false and may be true only with `reply_to`; it is
invalid with `destination`. Strict schema parsing rejects unknown fields. There
is no user/name/alias field or discovery path. This uses the existing default-off
`slack_send` tool and `slack:send` grant; it adds no configuration switch or
Slack scope.
The source must be an ingress-verified U/W human from an eligible live create,
edit, or human-reaction selector, never a bot, USLACKBOT, replay, orphan, stale,
or rejected occurrence. The bridge performs no lookup.

Before reservation or I/O, revalidate agent/session/instance/config,
conversation/thread policy or dynamic-DM authority, installation, and route.
Authored text containing raw `<@`, `<!`, or `<#` is rejected. The wire request
uses `mrkdwn:true`, `link_names:false`, no broadcast, and prepends the generated
mention plus one ASCII space. Final body is at most 40,000 Unicode scalars and is
rejected rather than truncated.

Durable logical text excludes the native mention; persistence retains only the
safe boolean. Results, facts, and logs expose no mention/native ID. The complete
route, text, flags, and installation freeze before send and retry byte-identically.
Errors are categorical and non-oracular. Slack acceptance is not a delivery,
read, or notification guarantee.

The governing choice is
[DECISION-tau-ext-slack-safe-source-mentions](DECISION-tau-ext-slack-safe-source-mentions.md).
