# DECISION-tau-ext-slack-conversation-discovery: Bounded configured-route discovery

Authority: confirmed, 2026-07-14, dpc

`slack_conversations` is a disabled-by-default, separately authorized,
config-only discovery surface. It replaces route catalogs embedded in every
prompt schema and grants no routing or send authority. Results are informational:
send re-resolves a current alias under normal tool and lifecycle authorization.

On-demand discovery trades one bounded tool call for removing route-count-scaled
schema tokens from every model turn. Exact pagination, fields, exclusions, and
validation are
[SPEC-tau-ext-slack-conversation-routing](SPEC-tau-ext-slack-conversation-routing.md).
