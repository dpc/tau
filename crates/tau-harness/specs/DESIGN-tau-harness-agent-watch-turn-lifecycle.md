# DESIGN-tau-harness-agent-watch-turn-lifecycle: Agent watch outer agent-turn lifecycle

Status: confirmed, 2026-07-10, dpc

`agent_watch` observes the canonical two-state outer agent turn: idle versus
running from activating input through the terminal response or termination.
Inner model rounds and intervening tool rounds remain in the same agent turn.
A new watch receives one
initial snapshot; genuine transitions are receiver-only durable notifications
with subscription identity and watched-agent runtime generation. Content
forwarding remains limited to direct user prompts and final responses.
Lifecycle-notification-only turns suppress both state edges to prevent cyclic
watch amplification. If ordinary input joins such a running generation, a
delayed start is emitted before the eventual matching stop.

Enable lifecycle classification and watch mutation form one authoritative
harness-loop operation. Only a Live target can create topology, subscription,
or notification state; Stopped and Unknown failures change none of that state.
A same-id reload remains unwatched until an explicit enable creates a fresh
subscription, while disable stays idempotent for known stopped endpoints.

The initial snapshot remains a durable client-visible fact but is not queued or
replayed into the watching model's context. Live delivery and transcript replay
derive later model-visible transition wording from the structured watched-turn
payload and watched-agent identity. The durable compatibility `message` text is
not authoritative presentation or model input.
