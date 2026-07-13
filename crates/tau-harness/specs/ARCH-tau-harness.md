# ARCH-tau-harness: tau-harness architecture

Provider account quota is an ephemeral current-state cache. The harness accepts
it only when every effective model route in the provider namespace has one
unambiguous extension owner, and every binding names a route won by that owner.
Split namespace ownership fails closed rather than letting one account snapshot
erase another source's state. Ownership or model-route loss clears associated
state; a sequence tombstone permits a later full replacement from the restored
owner—including an unretired epoch rotated while authority was absent—to
recover without accepting sparse state out of context. Explicit clears consume
matching tombstones and retire their epoch. The harness validates bounds plus epoch/sequence
transitions and projects full snapshots to live and late UI subscribers without
rebasing observation clocks. It never enters semantic session or agent history. See
[DESIGN-provider-quota-pacing](../../../specs/DESIGN-provider-quota-pacing.md).

This component implements the harness-owned parts of [SPEC-agent-watch](../../../specs/SPEC-agent-watch.md), [SPEC-compaction-and-context-recovery](../../../specs/SPEC-compaction-and-context-recovery.md), and [ARCH-external-message-boundary](../../../specs/ARCH-external-message-boundary.md).

## Canonical transport message boundary

Extensions register a transport family, send tool, and zero or more exact
proactive alias capabilities. Registration is bound to the authenticated
connection and current session generation; refresh replaces the route set, while
tool unregister, disconnect, and session rollover revoke it. Dedicated
ingress/send-completion RPCs replace generic event emission; the harness stamps
instance, agent endpoint, trust class, canonical id, and commit time, then owns
the protected durable fact. Per-peer retention is capped at 16 distinct transport
capabilities, in addition to the route-count and encoded-metadata bounds on each
registration.

Deduplication precedes publication and a bounded index is lazily rebuilt from
typed transcript entries. Source sequence checks are scoped by extension,
transport, conversation, and thread; durable append order remains authoritative.
Only the live post-commit hook acknowledges ingress and activates the runtime
route. It queues message identity, durable sequence, and an optional transcript
node; tool-adjacent messages remain unresolved until tool closure. Replay
reconstructs unacknowledged typed wakes from their durable identities and
inference watermarks rather than replaying live acknowledgements.

Remote transport acceptance cannot be transactional with Tau storage.
Successful-send completion validates the live call and either opaque reply route
or exact alias/endpoint/native conversation/fixed-thread capability, then queues
the outgoing fact immediately before its terminal tool result and caches exact
retry results. A crash can still leave remote and local state different; the
recorded acceptance must not imply delivery or read receipt.
The durable outgoing fact records authorization and tool-call audit before the
terminal result. Capability input is bounded and duplicate native
conversation/thread routes are rejected independently of presentation metadata.

`tau-harness` owns the daemon-side control plane for Tau sessions. It connects
clients and extensions, sequences events, applies interception, persists durable
session/agent facts, and delivers committed events to subscribers.

The harness also owns the bounded, redacted discovery snapshots specified by
[SPEC-tau-harness-peer-discovery](SPEC-tau-harness-peer-discovery.md).
Runtime metadata advertises only an untrusted entrypoint hint; the live harness
confirms its current session and effective policy through a narrow probe.
The same event loop owns peer entrypoint admission, selection, and explicit-role
auto-start. It admits bounded count/bytes/rate before creation, treats pending and
busy eligible agents as reusable endpoints, and releases sender success only from
the receive projection's post-commit continuation. This state is generation-bound
and in-memory; crash ambiguity follows best-effort at-least-once semantics.
The peer-created endpoint purpose itself is ordinary durable lifecycle state: the
harness embeds a reserved, non-inheritable metadata marker in the immutable
ordered `AgentStarted` creation fact and restores it before extension-query
teardown classification. Interception cannot drop or rewrite the protected
creation fact, and general metadata intake cannot set, unset, or inherit this key.

## Watch ownership

The harness owns the live topology, endpoint retirement, sanitized provider-work snapshots, and notification fanout specified by [SPEC-agent-watch](../../../specs/SPEC-agent-watch.md). Display labels remain separate from topology.


## Skills

The harness owns canonical discovered-skill state. Extensions such as `tau-ext-shell` announce candidate skill files, but the harness validates names/descriptions, resolves collisions by selected winner, stores user/model invocation flags, and builds model-visible prompt/tool snapshots from the current winners. `disable-model-invocation` removes a winner from `<available_skills>` and from the internal `skill` tool snapshot, and makes it user-invocable; it is a prompt-surface policy, not a filesystem security boundary.

User `/skill <name> [args]` and `/skill:<name> [args]` expansion is performed at harness prompt intake for both existing-agent prompts and new-agent initial prompts. Unknown, invalid, unreadable, or non-user-invocable commands emit `harness.notice` and are not submitted as model prompts. Successful invocations read a bounded skill-file prefix, strip frontmatter, and store the expanded Pi-style `<skill>` block in the normal prompt transcript.

Extensions that register with `extension.session_context_provider_register`,
subscribe to `session.started`, and publish session-wide prompt context such as
skills and AGENTS.md files must acknowledge completion with
`extension.session_context_ready`; eager startup waits for that acknowledgement
before considering startup discovery complete. Plain `session.started`
subscribers and per-agent-only context providers are not waited on unless they
explicitly register as session context providers. Role `required_skills`
validation runs after that startup/session skill discovery has completed. The
harness checks exact skill names against the selected model-visible skill
winners and verifies that the winning source is loadable with the same bounded
read/frontmatter rules as the `skill` tool. Roles with missing, hidden, or
unreadable required skills are removed from role selection/delegation and get a
mandatory replayable `harness.config_error` notice; if the selected/default
startup role is removed, startup fails rather than falling back silently.

## Daemon and provider reliability boundaries

The harness daemon listener is local IPC for trusted same-user Tau clients and
runtime discovery. Listener ownership and cleanup must preserve the socket
identity checks in `tau-socket`; a daemon-owned listener should outlive cloned raw
listener fds used by accept-forwarder threads, and socket-activated listeners must
not be unlinked by the harness.

Accept-loop shutdown must use an owned wake/cancellation primitive tied to the
accept thread, not polling sleeps and not the filesystem socket pathname. Runtime
socket paths can be removed or replaced while a cloned listener fd remains live, so
shutdown correctness must not depend on reconnecting to that path. Internal wake
traffic is control-plane state only and must never be forwarded as a harness
client.

The harness validates provider prompt ownership and derives public routing identity, but providers retain streaming and response-throughput authority under [SPEC-provider-response-streaming](../../../specs/SPEC-provider-response-streaming.md). Public stats are content-free and transient; they never become transcript, editor, prompt-stdin, or final-response content.
