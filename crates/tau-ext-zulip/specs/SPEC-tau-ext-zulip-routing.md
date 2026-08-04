# SPEC-tau-ext-zulip-routing: Zulip routing and authority

## Record justification

The routing contract spans configuration validation, queue admission, runtime ownership, source replies, proactive discovery, outbound sends, mutations, and lifecycle retirement, so no single implementation artifact owns it coherently.

Ingress requires an allowlisted numeric sender and exactly one current registered Tau agent. Direct messages require explicit DM policy and freeze the sorted non-bot participant set. Stream traffic requires an exact configured stream ID; an optional configured topic narrows the route. `mentions_only` additionally requires Zulip's structured `mentioned` flag. More than one matching agent fails closed.

An admitted base message establishes one opaque `MessageFactId` mapped to its agent, native message ID, and frozen conversation. This map is bounded and process-local. Replies and reactions accept only that reference and same-agent authority. Delete report submission revokes it. Native IDs, conversation facts, event order, text, aliases, and discovery output never select native routes.

Proactive sending requires a current exact alias with `proactive_send:true` and an exact topic. Discovery exposes aliases, topic labels, kinds, and trusted descriptions but excludes stream IDs, participant IDs, native messages, queue state, credentials, registrations, and health. Receive and proactive-send grants remain independent.

Every network completion rechecks configuration and registration generations before installing local authority or reporting success. If authority changed during provider I/O, the tool reports an unknown completed-remote-effect error and installs no new local capability. Reports contain no credentials or actionable native authority.
