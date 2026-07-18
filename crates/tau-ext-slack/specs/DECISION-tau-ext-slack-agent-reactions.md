# DECISION-tau-ext-slack-agent-reactions: Agent-authored Slack reactions

Authority: confirmed, 2026-07-14, dpc

The default-off, separately role-authorized reaction tool may mutate only an
exact retained Tau-issued message reference owned by the same currently live
agent, route, and Slack instance. It never accepts arbitrary Slack coordinates,
adopts remote state, persists actionable authority, or retries automatically.

Runtime revalidation and local ownership prevent the model from turning a stale
or borrowed reference into ambient Slack authority. The tradeoff is bounded
best-effort references that disappear on retirement or eviction.

Exact eligibility, ownership, limits, API, errors, and lifecycle are
[SPEC-tau-ext-slack-agent-reactions](SPEC-tau-ext-slack-agent-reactions.md).
