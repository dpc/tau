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
routes a policy-permitted verified human's add/remove reaction only to the registered agent
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

## Edits require remembered committed ingress ownership

Status: inferred

Slack `message_changed` is not fresh text ingress. A bounded runtime index of
commit-confirmed `(channel, ts)` identities binds the mutation to its original
agent, canonical id, sender, conversation, and thread. Consistent edits append
immutable typed operations; unknown, evicted, or conflicting edits fail closed
without a replacement create.


## Sender admission is independent from trigger scope and content trust

Status: unconfirmed

Strict mode is the default and admits only allowlisted verified humans. Lax mode accepts the increased prompt-injection exposure of other Slack-verified non-bot humans only in configured channels or an already-linked DM. This changes sender admission, not content trust: payloads remain untrusted, and identity plus `Allowlisted`/`LaxPermitted` policy are typed separately. Lax senders cannot link DMs or use agent-selection and bridge-control commands. Accepted ingress activates only an opaque source-bound reply route for the authenticated actor, conversation, and thread; it grants no arbitrary destination selection. Mentions-only/all-messages trigger scope is orthogonal and must preserve these sender, conversation, control, and route invariants.
