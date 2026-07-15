# DESIGN-tau-ext-slack-canonical-reply-selectors: Canonical reply selectors replace prompt lifecycle correlation

Status: inferred

New Slack traffic is one durable typed incoming occurrence and never a legacy prompt node. Only an exact validated protocol-v11 Committed+Active result binds its opaque canonical id and first canonical snapshot to a private source route. Inactive, replayed, rejected, orphaned, or mismatched results install nothing. Replies require the exact id; proactive sends require a configured alias, so queued or coalesced work cannot derive a native destination from prompt text or arrival order. This follows [DESIGN-canonical-transport-ingress](../../../specs/DESIGN-canonical-transport-ingress.md).
