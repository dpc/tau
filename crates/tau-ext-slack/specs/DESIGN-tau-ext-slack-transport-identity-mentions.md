# DESIGN-tau-ext-slack-transport-identity-mentions: Recognize the Slack bridge identity

Status: confirmed, 2026-07-15, dpc; fact-projection portions superseded
2026-07-17 by
[SPEC-extension-published-message-facts](../../../specs/SPEC-extension-published-message-facts.md)

## Decision

Incoming Slack text may refer to the authenticated receiving bot with the exact
semantic token `@slack_bridge`. The token is orientation text shared by Slack
instances, not a Tau agent identity, native Slack identifier, selector,
capability, or routing grant.

Classification uses only the exact validated `auth.test.user_id` paired with the
admitted event's installation and lifecycle. It recognizes exact case-sensitive
`<@U…>`/`<@W…>` entities outside complete equal-length backtick ranges. Escaped,
labeled, partial, malformed, case-changed, lookalike, other-user, and literal
`@slack_bridge` text do not count.

Exactly one eligible leading occurrence is removed for command/routing-prefix
compatibility. Remaining eligible occurrences become literal `@slack_bridge` in
the published text. Bridge-local commands publish no fact. Creates and edits
classify their own current text; reactions and deletes carry no text.

The generic message-fact schema has no transport-identity-mentioned field.
Slack currently does not persist the classifier result in `extension_data`; only
the normalized text survives publication. The removed envelope attribute and
its protocol/harness projection tests are not current requirements.

Successful `slack_register(enabled:true)` returns:

```json
{"status":"registered","incoming_transport_reference":"@slack_bridge"}
```

Unregister returns `{"status":"unregistered"}`. No model-facing result exposes
the bot or workspace ID. On egress `@slack_bridge` stays literal. Raw `<@`,
`<!`, and `<#` controls remain rejected, while `mention_source_user` is the only
generated native user mention.

Slack tests own exact entity parsing, code-range negatives,
create/edit/command/route behavior, registration output, and literal egress.
