# DECISION-tau-ext-slack-transport-identity-mentions: Safe bot-mention normalization

Authority: confirmed, 2026-07-15, dpc

Authenticated mentions of the installed Slack bot normalize to semantic
`@slack_bridge` orientation text. That text is never identity, a selector, a
capability, or authority. Native bot/workspace IDs are not exposed to the model,
and the mention classifier result is not persisted.

Exact mention recognition, code-span handling, command removal, publication,
replay, and egress behavior is
[SPEC-tau-ext-slack-ingress](SPEC-tau-ext-slack-ingress.md).
