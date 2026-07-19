# DECISION-tau-ext-slack-single-agent-operating-model: One receiving agent per Slack instance

Authority: confirmed, 2026-07-17, dpc

One configured `std-slack` extension instance is intended to serve one receiving
Tau agent at a time. That agent may use multiple configured conversations,
threads, and dynamic direct-message links, but the extension instance is not a
strong multi-agent router.

Any multi-agent use or retargeting is unsupported best-effort behavior and
provides no exact routing, once-only delivery, or cross-agent deduplication
guarantee. This is an operating-model choice, not a runtime prohibition; dependable
deployments use separate configured instances for separate receiving agents. The
tradeoff is simpler extension-local ownership at the cost of unsupported sharing.
