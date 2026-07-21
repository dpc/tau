# ARCH-tau-harness: tau-harness architecture

## Status

The external-message, provider-model, provider-quota, provider-execution,
tool-lifecycle, tool-request, tool-progress, terminal-tool-outcome, prompt-fragment, and
session-discovery slices now use generic `Emit` publication, immutable authenticated
internal publisher snapshots, source-aware admission, and downstream processing as required by
[DECISION-generic-peer-event-emission](../../../specs/DECISION-generic-peer-event-emission.md).
The general protocol-level authenticated publisher envelope and remaining peer
event families remain to be migrated.

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
Configure. Configured Tool/Core peers publish transient
`tool.*_declared` events; only a post-commit consumer validates replacements,
mutates the registry, and publishes protected harness-authored canonical
`tool.register` / `tool.unregister` state with configured instance provenance.
The harness owns final-name validation and deterministic startup collision
resolution. Extensions retain declaration and tool-specific semantic ownership.
The exact flow is
[SPEC-tool-declarations-and-canonical-state](../../../specs/SPEC-tool-declarations-and-canonical-state.md).
Tool/Core peers likewise submit `tool.progress_reported` observations through
ordinary generic publication. Only the post-commit consumer validates the
captured live routed-call owner and background state, then publishes protected
harness-sourced `tool.progress`; see
[SPEC-tool-progress-reports-and-canonical-facts](../../../specs/SPEC-tool-progress-reports-and-canonical-facts.md).
They submit terminal result, error, and cancellation reports through the same
generic commit boundary. The post-commit consumer revalidates the captured live
generation and exact routed-call owner before applying existing terminal
processing and publishing protected harness-sourced terminal, provider, or
background projections; see
[SPEC-terminal-tool-reports-and-canonical-outcomes](../../../specs/SPEC-terminal-tool-reports-and-canonical-outcomes.md).
Configured Provider/Tool/Core peers submit `tool.request` through generic
publication before routing. The post-commit consumer revalidates the captured
generation and call-id correlation, installs terminal ownership, and publishes
harness-sourced started or rejection/terminal facts. Caller-selected durable
requests retain stable configured publisher provenance but never rerun work on
replay; see
[SPEC-tool-requests-and-routing](../../../specs/SPEC-tool-requests-and-routing.md).

Configured Provider execution uses the same generic commit boundary. Five `_reported`
observations commit before exact generation and prompt/retry correlation; the harness
then publishes canonical provider facts or a requester-directed retry outcome. Terminal
response alternatives retain the existing recovery, persistence, tool dispatch, and turn
closure pipeline. See
[SPEC-provider-execution-reports-and-canonical-facts](../../../specs/SPEC-provider-execution-reports-and-canonical-facts.md).
Peer requests routed to harness-internal tools use separate runtime loaded-agent
correlation for execution, wait, ephemeral, and unload lifecycle; they never
acquire transcript tool-call ownership, so their terminal facts remain
ownerless and non-transcript.
For each prompt, the harness alone resolves the effective
post-policy/provider-filtered tool snapshot used for definitions,
authorization, capabilities, and diagnostics, as specified by
[SPEC-tau-harness-prompt-dispatch](SPEC-tau-harness-prompt-dispatch.md).

The harness persists generic per-agent extension metadata commits. A shell
extension instance uses its configured name to own one workdir namespace and
publishes context from committed metadata; exact behavior is
[SPEC-per-agent-extension-workdirs](../../../specs/SPEC-per-agent-extension-workdirs.md).

## External-message reports and canonical facts

Extensions publish six transient `message.*_reported` events through generic
`Emit` publication and interception. A downstream post-commit consumer stamps
the authenticated extension's stable configured name and publishes the
corresponding immutable, must-pass canonical `message.*` fact. Canonical commit
persists the fact in the target agent journal (or session fallback journal for
unknown targets) before broadcast. Consumers cannot reject, replace, or mutate
a committed canonical fact. The harness owns no transport registration,
admission, ordering, deduplication, native routing, reply state, or
send-completion protocol.

The post-commit prompt consumer validates universal fields, projects valid
incoming facts as ordinary user context, and requests one live activation after
transcript placement. `message.sent` becomes assistant context without activation.
Open tool rounds defer transcript placement and wake until terminal tool results
close, while the fact itself broadcasts immediately. Replay reconstructs context
but never wakes the agent, resends transport traffic, or rebuilds
extension-private authority. Invalid or unavailable targets remain committed and
visible to subscribers even when no prompt projection is possible.
The complete schema, persistence, and projection contract is
[SPEC-external-message-reports-and-facts](../../../specs/SPEC-external-message-reports-and-facts.md).

## Provider model declarations and canonical state

Only authenticated configured provider extensions may publish transient,
interceptable `provider.models_declared` replacement declarations. The generic
publication envelope snapshots the configured connection and provider kind so
parking, disconnect, or replacement cannot substitute publisher identity.
Post-commit processing stages startup declarations until activation or publishes
protected harness-authored `provider.models_updated` current state before applying
the existing route, collision, availability, and restored-work reconciliation.
Canonical model state cannot be dropped or rewritten; the existing availability
projections retain their existing interception behavior. Each canonical snapshot
also carries the stable configured provider publisher so replacement and empty
snapshots remain attributable even though their delivery source is the harness.
Subscribe-time current-state replay synthesizes canonical updates with that stable
publisher and harness source metadata only; it never replays declarations or reruns
their side-effects. The payload and event-name contract is documented in
[SPEC-tau-proto-provider-data](../../tau-proto/specs/SPEC-tau-proto-provider-data.md#provider-model-declarations-and-canonical-state).

Configured Provider peers publish explicitly transient
`provider.quota_*_reported` observations through ordinary generic publication.
Only the post-commit consumer revalidates the captured live generation,
provider/route ownership, bounds, and epoch/sequence transition before mutating
ephemeral current state and publishing protected harness-sourced
`harness.provider_quota_changed`. See
[SPEC-provider-quota-pacing](../../../specs/SPEC-provider-quota-pacing.md).

`tau-harness` owns the daemon-side control plane for Tau sessions. It connects
clients and extensions, sequences events, applies interception, persists durable
session/agent facts, and delivers committed events to subscribers.

The harness also owns the bounded, redacted discovery snapshots specified by
[SPEC-tau-harness-peer-discovery](SPEC-tau-harness-peer-discovery.md).
Runtime metadata advertises only an untrusted entrypoint hint; the live harness
confirms its current session and effective policy through a narrow probe.
The same event loop owns inter-session receiver admission, fair live selection,
and configured-order role auto-start. It admits bounded count/bytes/rate before
creation, treats pending and busy eligible agents as reusable endpoints, and
releases sender success only from the receive projection's post-commit
continuation. This state is generation-bound and in-memory; crash ambiguity
follows best-effort at-least-once semantics.
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
Session registration, skills, AGENTS.md contents, and readiness cross ordinary
interception and commit before they update these projections or release the barrier.
See
[SPEC-session-discovery-declarations-and-readiness](../../../specs/SPEC-session-discovery-declarations-and-readiness.md).

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

Extension prompt-fragment declarations cross ordinary interception and commit
before the harness replaces the exact configured connection's runtime
source/name projection. Pre-Ready declarations reserve activation capacity and
block Ready while parked; prompt assembly consumes only committed active
fragments. See
[SPEC-prompt-fragment-declarations-and-projection](../../../specs/SPEC-prompt-fragment-declarations-and-projection.md).

The harness daemon listener is local IPC for trusted same-user Tau clients and
runtime discovery. Listener ownership and cleanup must preserve the socket
identity checks in `tau-socket`; a daemon-owned listener should outlive cloned raw
listener fds used by accept-forwarder threads, and socket-activated listeners must
not be unlinked by the harness.

Discovery is non-destructive because liveness and filesystem identity checks
cannot be made atomic with PID reuse and listener replacement. An owned CLI
first closes its initial-client transport and gives the daemon's
exit-on-disconnect path a bounded grace period to shut down and remove its own
runtime pair; forced termination is only the fallback. Targeted session lookup
may traverse a larger bounded raw catalog than general peer discovery so stale
unrelated pairs do not consume the much smaller matching-candidate budget.
Local running-session listing isolates bounded runtime-path traversal, then uses
a per-candidate, correlation-matched local socket RPC to obtain each responsive harness's
in-memory current session id. Runtime metadata and persisted session directories
are not lifecycle authority. The overall scan has a fixed deadline and fails
instead of returning a partial snapshot when candidate traversal or the total
probe budget is incomplete.
This authority is governed by
[DECISION-current-session-control-rpc](../../../specs/DECISION-current-session-control-rpc.md).

Accept-loop shutdown must use an owned wake/cancellation primitive tied to the
accept thread, not polling sleeps and not the filesystem socket pathname. Runtime
socket paths can be removed or replaced while a cloned listener fd remains live, so
shutdown correctness must not depend on reconnecting to that path. Internal wake
traffic is control-plane state only and must never be forwarded as a harness
client.

The harness validates provider prompt ownership and derives public routing identity, but providers retain streaming and response-throughput authority under [SPEC-provider-response-streaming](../../../specs/SPEC-provider-response-streaming.md). Public stats are content-free and transient; they never become transcript, editor, prompt-stdin, or final-response content.

## Agent navigation authority

Current-session runtime owns loaded-agent navigation modes alongside membership
and routing. Modes affect UI eligibility only, never loading, routing, delivery,
watches, execution, or model behavior.

The authoritative rationale and lifecycle are recorded in
[DECISION-harness-owned-agent-navigation-modes](../../../specs/DECISION-harness-owned-agent-navigation-modes.md).

## Directed agent roster

The event loop owns a bounded, read-only current-session roster RPC for local UI
connections. It reads current and ever-loaded caches atomically seeded from
validated committed membership before runtime restoration and updated only after
later membership commits. Restore/commit failures invalidate the projection.
The RPC checks the entry limit before cloning ids, joins live runtime/navigation
state, then adds shallow bounded creation facts.
Results are correlated and requester-directed; they are not events and never
enter persistence, interception, publication, subscription replay, or extension
delivery. Exact wire behavior is specified by
[SPEC-tau-proto-session-events](../../tau-proto/specs/SPEC-tau-proto-session-events.md).
