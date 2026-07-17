# DESIGN-tau-ext-slack-canonical-reply-selectors: Canonical reply selectors replace prompt lifecycle correlation

Status: inferred

New Slack traffic publishes one immutable message fact and never a legacy prompt
node. Successful publication binds its Tau-issued message-fact ID to the private
source route; failed publication and replay install nothing. Replies require the exact id and proactive
sends require a configured alias, so queued or coalesced work cannot derive a
native destination from prompt text or arrival order. The route is runtime-only,
and retargeting or races are explicitly best-effort under
[DESIGN-tau-ext-slack-single-agent-operating-model](DESIGN-tau-ext-slack-single-agent-operating-model.md).
