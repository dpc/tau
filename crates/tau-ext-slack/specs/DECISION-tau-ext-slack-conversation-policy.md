# DECISION-tau-ext-slack-conversation-policy: Unified exact conversation policy

Authority: confirmed, 2026-07-14, dpc

## Decision

Slack receive and initiation policy uses bounded, exact routes named by stable
aliases. Receive and proactive-send authority are independent. Dynamic
direct-message authority is separate, explicit, and bounded. Proactive callers
select aliases rather than native identifiers.

## Rationale

Explicit exact grants avoid asymmetric global authority. Exact behavior is
specified by
[SPEC-tau-ext-slack-conversation-routing](SPEC-tau-ext-slack-conversation-routing.md).
