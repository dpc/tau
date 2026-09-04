# SPEC-tau-ext-zulip-routing: Zulip routing and authority

## Record justification

The routing contract spans configuration validation, queue admission, runtime ownership, source replies, proactive discovery, outbound sends, mutations, and lifecycle retirement, so no single implementation artifact owns it coherently.

Ingress requires an allowlisted numeric sender and exactly one current registered Tau agent. Direct messages require explicit DM policy and complete `private`/`direct` participant evidence: at most 33 unique recipient objects with nonzero numeric IDs, including the authenticated queue bot and parsed sender. Admission removes that bot, requires 1–32 allowlisted non-bot IDs, then freezes their sorted set; malformed or incomplete evidence creates neither a report nor reply authority. Each configured stream name resolves to one native ID before queue registration; inbound stream traffic matches that ID, and an optional configured topic narrows the route. `all_messages` also subscribes the bot to its configured channel before every queue registration, never on route removal. A topicless agent-chosen proactive route may also receive every topic, but that receive grant cannot overlap another receive route in the stream; a send-only such route may coexist with an exact-topic receive route. `mentions_only` additionally requires Zulip's structured `mentioned` flag. Admission does not remove or otherwise interpret inbound Markdown, including a leading bot address. More than one matching agent fails closed.

When `non_allowlisted_activity: {}` is configured, a created stream message
that passes every preceding syntax, size, queue, generation, duplicate, self,
route, topic, mention, and single-agent predicate but fails only sender
allowlisting increments a bounded same-conversation count and emits no report.
Direct messages never contribute. The conversation scope is the keyed stable
identity of exact native `(stream_id, topic)`; retained sender identity is the
private numeric ID, never the display name. A later fully admitted message
flushes only its same-scope bucket when a complete note fits beside the exact
body. Otherwise it delivers unchanged and retains the bucket. Queue
replacement and every configuration or registration authority change clear
all buckets.

Catch-up applies this same current sender and receive policy to newly created
history messages. Skipped history may advance the position; an allowed and
routed message may advance it only after canonical post-commit self-observation.
Policy changes do not retroactively scan before the stored position.

Opt-in send-only mode has no ingress. It requires exactly one proactive direct-message route and rejects nonempty sender allowlists, sender aliases, conversations, direct-message receive policy, and offline catch-up. It declares only `zulip_send` without the Zulip tool group; register, discovery, and reaction calls fail closed if delivered anyway. It creates no queue or event worker, processes no injected event even if it names a locally sent native message, publishes no Zulip-originated report, and installs no source reply or mutation ownership. Switching between ordinary and send-only modes requires extension restart.

An admitted base message establishes one opaque `MessageFactId` mapped to its agent, native message ID, and frozen conversation. This map is bounded and process-local. Replies and reactions accept only that reference and same-agent authority. Delete report submission revokes it. Native IDs, conversation facts, event order, text, sender presentation aliases, and discovery output never select native routes. Only current operator-configured proactive stream names and proactive-DM aliases select their fixed extension-private routes.

Proactive sending requires a current configured destination. A stream name additionally requires `proactive_send:true` and by default has one exact configured topic. An operator may instead grant `agent_chosen_topic:true` only on a proactive stream name that omits its configured topic; that destination accepts one bounded caller topic while keeping its resolved stream ID private. A proactive direct-message alias has one fixed nonzero configured recipient, independent of ingress sender allowlists; it never accepts a caller recipient or caller topic. The caller topic may be Zulip's canonical empty general-chat topic. Caller topics are rejected for exact-topic names, direct-message aliases, and source-bound replies. Discovery exposes configured names, topic labels, kinds, explicit agent-chosen-topic authority, and trusted descriptions but excludes stream IDs, participant IDs, native messages, queue state, credentials, registrations, and health. Receive and proactive-send grants remain independent.

Every network completion rechecks configuration and registration generations before installing local authority or reporting success. If authority changed during provider I/O, the tool reports an unknown completed-remote-effect error and installs no new local capability. Reports contain no credentials or actionable native authority.

Send-only proactive sending does not require agent registration. Its narrowed send declaration accepts `message` and the sole configured `destination` alias; runtime rejects replies, topics, unknown aliases, and any numeric or caller-supplied recipient. Existing ordinary-mode registration and send behavior remains unchanged.
