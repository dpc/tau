# DECISION-tau-ext-slack-conversation-policy: Unified exact conversation policy

Authority: confirmed, 2026-07-14, dpc

Slack receive and initiation policy uses one bounded `conversations` list. Each
record binds a stable alias to one exact native conversation and optional fixed
thread, declares its explicit `channel`, `mpim`, or `dm` kind, and independently
enables `receive` and `proactive_send`. Dynamic direct-message discovery is a
separate explicit, bounded, exact-user-bound policy.

Aliases, not native identifiers, are proactive selectors. Receive creates
source-bound reply authority but no proactive authority; proactive send grants no
receive or reply authority. Current policy is revalidated at use time.

This trades explicit configuration for atomic conflict validation and avoids
asymmetric global grants. Exact behavior is specified by
[SPEC-tau-ext-slack-conversation-routing](SPEC-tau-ext-slack-conversation-routing.md).
