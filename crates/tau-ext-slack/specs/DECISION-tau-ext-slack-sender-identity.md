# DECISION-tau-ext-slack-sender-identity: Native Slack identity is authoritative

Authority: confirmed, 2026-07-15, dpc

Verified native Slack user identity is sender authority. Display names and
configured aliases are presentation only and cannot affect admission, commands,
routing, replies, reactions, deduplication, or selection. Published identity is
opaque and installation-scoped; actionable native identity remains bridge-local.

Exact verification, bounds, lifecycle, replay, and projection behavior is
[SPEC-tau-ext-slack-ingress](SPEC-tau-ext-slack-ingress.md).
