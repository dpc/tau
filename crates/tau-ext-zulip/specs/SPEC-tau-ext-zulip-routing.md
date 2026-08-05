# SPEC-tau-ext-zulip-routing: Zulip routing and authority

## Record justification

The routing contract spans configuration validation, queue admission, runtime ownership, source replies, proactive discovery, outbound sends, mutations, and lifecycle retirement, so no single implementation artifact owns it coherently.

Ingress requires an allowlisted numeric sender and exactly one current registered Tau agent. Direct messages require explicit DM policy and freeze the sorted non-bot participant set. Stream traffic requires an exact configured stream ID; an optional configured topic narrows the route. A topicless agent-chosen proactive route may also receive every topic, but that receive grant cannot overlap another receive route in the stream; a send-only such route may coexist with an exact-topic receive route. `mentions_only` additionally requires Zulip's structured `mentioned` flag. More than one matching agent fails closed.

Catch-up applies this same current sender and receive policy to newly created
history messages. Skipped history may advance the position; an allowed and
routed message may advance it only after canonical post-commit self-observation.
Policy changes do not retroactively scan before the stored position.

An admitted base message establishes one opaque `MessageFactId` mapped to its agent, native message ID, and frozen conversation. This map is bounded and process-local. Replies and reactions accept only that reference and same-agent authority. Delete report submission revokes it. Native IDs, conversation facts, event order, text, sender presentation aliases, and discovery output never select native routes. Only current operator-configured proactive destination aliases select their fixed extension-private routes.

Proactive sending requires a current configured alias. A stream alias additionally requires `proactive_send:true` and by default has one exact configured topic. An operator may instead grant `agent_chosen_topic:true` only on a proactive stream alias that omits its configured topic; that alias accepts one bounded caller topic while keeping its configured stream ID private. A proactive direct-message alias has one fixed nonzero configured recipient, independent of ingress sender allowlists; it never accepts a caller recipient or caller topic. The caller topic may be Zulip's canonical empty general-chat topic. Caller topics are rejected for exact-topic aliases, direct-message aliases, and source-bound replies. Discovery exposes aliases, topic labels, kinds, explicit agent-chosen-topic authority, and trusted descriptions but excludes stream IDs, participant IDs, native messages, queue state, credentials, registrations, and health. Receive and proactive-send grants remain independent.

Every network completion rechecks configuration and registration generations before installing local authority or reporting success. If authority changed during provider I/O, the tool reports an unknown completed-remote-effect error and installs no new local capability. Reports contain no credentials or actionable native authority.
