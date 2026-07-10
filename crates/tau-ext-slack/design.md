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

## Canonical reply selectors replace prompt lifecycle correlation

Status: inferred

New Slack traffic is one durable typed incoming occurrence and never a legacy prompt node. The harness commit result returns an opaque canonical id; the bridge binds that id to its private source route. `slack_send` requires the exact id, so queued or coalesced work cannot select a destination from prompt text or arrival order. Replay and failed ingress never activate a route.

## Reactions require remembered bridge post ownership

Status: unconfirmed

Reaction events are not general Slack-channel ingress. The bridge remembers a
bounded set of message identities returned by successful `slack_send` calls and
routes an allowlisted human's add/remove reaction only to the registered agent
that created that exact post in an authorized conversation. This state is
runtime-only; reactions to unknown or evicted posts fail closed.
The authoritative thread root comes from the authenticated outbound request.
Omitted thread metadata in the Slack post response or reaction is tolerated,
while conflicting metadata prevents ownership caching or reaction routing.

## Thread destinations come only from authenticated prompt origins

Status: unconfirmed

Slack thread roots are validated event metadata and travel inside the same
pending, accepted, and active conversation state as the configured channel or
linked DM. `slack_send` exposes no thread argument. Top-level origins store no
thread root, while threaded origins supply their root to `chat.postMessage`;
successful completion must repeat the exact typed route metadata or fail closed.
