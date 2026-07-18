# ARCH-tau-harness: tau-harness architecture

Architectural or externally meaningful functional changes to harness event
logs/journals or interfaces with extensions require the separately reviewed,
human-confirmed decision mandated by
[DECISION-persistence-and-extension-interface-change-approval](../../../specs/DECISION-persistence-and-extension-interface-change-approval.md).

Provider account quota is an ephemeral current-state cache. The harness accepts
it only when every effective model route in the provider namespace has one
unambiguous extension owner, and every binding names a route won by that owner.
Split namespace ownership fails closed rather than letting one account snapshot
erase another source's state. Ownership or model-route loss clears sensitive
windows and bindings; the harness retains and replays only an empty capability
snapshot until exit or an accepted replacement, keeping live and late clients
converged. A sequence tombstone permits a later full replacement from the
restored owner—including an unretired epoch rotated while authority was
absent—to recover without accepting sparse state out of context. Explicit
clears consume matching tombstones and retire their epoch. The harness validates
bounds plus epoch/sequence transitions and projects full snapshots to live and
late UI subscribers without rebasing observation clocks. It never enters
semantic session or agent history. See
[SPEC-provider-quota-pacing](../../../specs/SPEC-provider-quota-pacing.md).

This component implements the harness-owned parts of [SPEC-agent-watch](../../../specs/SPEC-agent-watch.md), [SPEC-compaction-and-context-recovery](../../../specs/SPEC-compaction-and-context-recovery.md), and [ARCH-external-message-boundary](../../../specs/ARCH-external-message-boundary.md).

## Tool-surface and extension-instance ownership

The harness assigns immutable per-instance tool-prefix envelopes through
Configure, validates registration and final-name ownership, and owns startup
collision resolution. Extensions retain declaration and tool-specific semantic
ownership. For each prompt, the harness alone resolves the effective
post-policy/provider-filtered tool snapshot used for definitions,
authorization, capabilities, and diagnostics, as specified by
[SPEC-tau-harness-prompt-dispatch](SPEC-tau-harness-prompt-dispatch.md).

The harness persists generic per-agent extension metadata commits. A shell
extension instance uses its configured name to own one workdir namespace and
publishes context from committed metadata; exact behavior is
[SPEC-per-agent-extension-workdirs](../../../specs/SPEC-per-agent-extension-workdirs.md).

## Extension-published message facts

Extensions publish the six immutable `message.*` fact types through ordinary
`Emit`. Intake unconditionally stamps the authenticated extension's stable
configured name, ignores the transient bit, persists the exact fact in the
target agent journal (or the session fallback journal for unknown targets), and
only then broadcasts it. Consumers cannot reject, replace, or mutate a committed
fact. The harness owns no transport registration, admission, ordering,
deduplication, native routing, reply state, or send-completion protocol.

The post-commit prompt consumer validates universal fields, projects valid
incoming facts as ordinary user context, and requests one live activation after
transcript placement. `message.sent` becomes assistant context without activation.
Open tool rounds defer transcript placement and wake until terminal tool results
close, while the fact itself broadcasts immediately. Replay reconstructs context
but never wakes the agent, resends transport traffic, or rebuilds
extension-private authority. Invalid or unavailable targets remain committed and
visible to subscribers even when no prompt projection is possible.
The complete schema, persistence, and projection contract is
[SPEC-extension-published-message-facts](../../../specs/SPEC-extension-published-message-facts.md).

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

The harness owns the live acyclic topology, endpoint retirement, sanitized provider-work snapshots, and notification fanout specified by [SPEC-agent-watch](../../../specs/SPEC-agent-watch.md) and [DECISION-agent-watch-acyclic-topology](../../../specs/DECISION-agent-watch-acyclic-topology.md). Display labels remain separate from topology.


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

`ServeOptions` has opt-in hermetic-test controls that bypass ambient startup
override transports and require an exact resolved extension-name set before any
configured child is spawned. Defaults preserve normal daemon configuration.
These controls constrain deterministic test composition; they are not an
extension sandbox or production security policy.

Configured extension children are trusted local executables with limited protocol
authority, not hostile transport peers. The controlling boundary is linked from
[`SECURITY.md`](../../../SECURITY.md) and
[`SPEC-tau-harness-session-state`](SPEC-tau-harness-session-state.md#extension-data).
Reviews must not conflate that boundary with external adapter payloads or
cooperative cross-harness messaging.

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
