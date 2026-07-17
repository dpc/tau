# DESIGN-tau-ext-slack-single-agent-operating-model: One receiving agent per Slack instance

Status: confirmed, 2026-07-17, dpc

One configured `std-slack` extension instance is intended to serve one receiving
Tau agent at a time. That agent may use multiple configured conversations,
threads, and dynamic direct-message links, but the extension instance is not a
strong multi-agent router.

The runtime may permit registrations to move between agents or may encounter
more than one agent using an instance during lifecycle transitions. That behavior
is ad hoc and best-effort. Operators and agents must not rely on it for exact
cross-agent routing, permanent occurrence ownership, once-only delivery, or
cross-agent deduplication. Retargeting, restart, cache eviction, or races may
route a retry differently or produce a duplicate.

This is an operating-model decision, not a newly enforced runtime restriction.
The extension does not reject every configuration or lifecycle sequence that can
involve multiple agents. Runtime enforcement requires a separate explicit
decision.

For a dependable deployment, assign one receiving agent to each configured Slack
extension instance and avoid retargeting that instance while it is active. When
separate agents need independent Slack bridges, configure separate extension
instances with distinct tool prefixes and route policy. Treat deliberate sharing
of one instance as unsupported best-effort behavior.
