# ARCH-external-message-boundary: External message boundary

Canonical external-message intake uses source-bound transport capability
registration and dedicated RPCs. Extension-provided labels or payload can never
claim `HumanUi`, harness-internal, authenticated Tau-agent, or another extension
instance authority. External payload always remains untrusted content; identity
assurance and allowlist/lax routing policy are separate fields.

Canonical `reply_to` ids are opaque selectors, not secrets or bearer
capabilities. Send completion revalidates the owning live connection, active
session generation, agent, reply tool, and originating route. Durable facts may
contain bounded native sender, conversation/thread, event, and message ids
visible to authorized event subscribers, but never transport credentials or raw
private route capabilities.

## Cross-harness messages

Cross-harness agent messages are local IPC between Tau harness daemons for the
same user. They use a dedicated external-message RPC rather than generic event
`emit`; extensions and ordinary UI clients cannot publish harness-owned
`agent.message_sent` / `agent.message_received` projections directly. The
receiving harness performs only bounded syntax and claimed-session checks before
it authenticates caller-supplied sender identity plus message/watch-response kind
by calling back to the claimed sender
harness with a sender-minted per-message capability bound to the message body and
routing fields. Runtime daemon metadata is discovery data only: `session_id`
means the daemon's current active session and is updated on `/session new`; stale
or ambiguous metadata must fail discovery rather than silently choosing a target.
A failed socket probe alone must not delete runtime discovery files while the
metadata pid is still live; dead-pid entries remain eligible for cleanup on
platforms where Tau has a safe pid-liveness backend.
The binding carries a tagged exact-agent or bare-entrypoint recipient. Bare
selection and exact-agent inventory validation happen only after callback
authentication and target-policy/session revalidation; exact known-address
behavior remains independent of entrypoint advertisement. Runtime lookup,
connection, callback, and send work is bounded by shared absolute deadlines and
non-queued process/connection admission. Disconnect or session rollover cancels
the associated work, and stale-generation completions cannot publish projections.

Peer-session discovery uses a metadata-schema-versioned `peer_entrypoint` hint
only to select bounded probe candidates. A live target RPC confirms the active
session and effective entrypoint before a session is returned. Discovery never
exposes socket paths, pids, full project roots, agent ids, prompts, tasks,
models, tools, or provider state. The authority split is confirmed by
[DESIGN-peer-entrypoints](DESIGN-peer-entrypoints.md).
