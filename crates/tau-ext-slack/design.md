# Design decisions

This file records major design decisions currently embodied by this directory's
code, and how authoritative each decision is. It is not an architecture overview,
ADR log, todo list, roadmap, implementation guide, or changelog.

## Slack lifecycle tests use local fakes

Status: inferred

Slack lifecycle behavior is tested without live Slack credentials. Unit tests use
fake `SlackClient` implementations for Web API calls and loopback websocket
servers for Socket Mode behavior. Production Socket Mode URLs still require
`wss`, while tests may use `ws://127.0.0.1` so shutdown, reconnect, framing, and
ack behavior can be exercised deterministically without external network access.

Regression tests should prefer this fake-client and loopback approach over real
Slack workspaces. When testing shutdown paths, drive the worker to the blocked
state being exercised, request shutdown through the shared signal, and bound the
post-request wait below any removed polling interval so polling regressions fail.

## Queued prompts fail closed across channels

Status: inferred

Slack reply authorization follows harness prompt lifecycle rather than arrival
order. A busy-agent Slack prompt may be queued and later folded as
`agent.prompt_steered` into a tool-result follow-up. The bridge authenticates the
steered agent, exact text, and private correlation id before retiring the
pending record. Because the follow-up mixes the prior turn with steered input
and has no single safe reply origin, any steer revokes `slack_send` authorization
instead of choosing a destination.

## Reactions require remembered bridge post ownership

Status: unconfirmed

Reaction events are not general Slack-channel ingress. The bridge remembers a
bounded set of message identities returned by successful `slack_send` calls and
routes an allowlisted human's add/remove reaction only to the registered agent
that created that exact post in an authorized conversation. This state is
runtime-only; reactions to unknown or evicted posts fail closed.
