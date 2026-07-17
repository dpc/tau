# DESIGN-tau-ext-slack-canonical-reply-selectors: Canonical reply selectors replace prompt lifecycle correlation

Status: inferred

New Slack traffic is one durable typed incoming occurrence and never a legacy
prompt node. A correlated accepted result binds its opaque message id to the
pending private source route; rejected, replayed, and orphaned results install
nothing. Replies require the exact id and proactive
sends require a configured alias, so queued or coalesced work cannot derive a
native destination from prompt text or arrival order. The route is runtime-only,
and retargeting or races are explicitly best-effort under
[DESIGN-tau-ext-slack-single-agent-operating-model](DESIGN-tau-ext-slack-single-agent-operating-model.md).
