# DESIGN-tau-ext-slack-canonical-reply-selectors: Canonical reply selectors replace prompt lifecycle correlation

Status: inferred

New Slack traffic is one durable typed incoming occurrence and never a legacy prompt node. The harness commit result returns an opaque canonical id; the bridge binds that id to its private source route. Replies require the exact id; proactive sends require a configured alias, so queued or coalesced work cannot derive a native destination from prompt text or arrival order. Replay and failed ingress never activate a route.
