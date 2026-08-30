# ARCH-tau-harness: tau-harness architecture

## Semantic persistence lifecycle

One Harness-owned `SemanticPersistenceOwner` prepares every durable agent,
ordinary-session, and restore stream. Startup and switch prepare `New` or strict
`Resume` authority before exposing runtime state. Switch closes the complete old
lease set under one deadline; failure after canonical shutdown fail-stops
persistence rather than forging a second owner.

Canonical publication first admits the semantic frame and complete in-memory
replacement. Only successful admission may advance EventLog/debug output, bus
delivery, reactions, or continuations. Worker I/O follows in one global FIFO.
Rollback-safe failures retain the accepted head; uncertain rollback poisons that
generation. Restart therefore sees the longest valid durable prefix.

The owner wakes the central runtime loop when bounded worker failures arrive or
previously full capacity becomes available. The harness drains content-free
failure and capacity transitions into operational tracing, then retries exact
retained publication owners; ordinary runtime progress remains a fallback.
These wakes carry no semantic event or payload and do not strengthen the
asynchronous crash boundary.

Live delivery uses one process-local logical stream with runtime-only positions.
The event bus freezes selector, visibility, exclusion, and directed-route
eligibility as consumer generations when admitting a frame. Per-connection
writers follow independent cursors and shared retention prunes after every
active generation advances or retires; a stalled connected generation remains
eligible and can pin retention. Replay pauses its follower at a captured live
tail until the replay-complete boundary. This architecture is governed by
[GATE-runtime-live-event-log-cursors](../../../specs/GATE-runtime-live-event-log-cursors.md)
and does not change semantic journals or protocol envelopes.

A default-off process-local diagnostic recursively estimates the canonical live
suffix once per shared allocation and separately reports strong-owner and
pending-consumer fanout. It creates no protocol, cursor, journal, or replay
identity and does not alter pruning or delivery. Requested capacity is a
diagnostic projection estimate; kernel buffers and allocator/RSS ownership
remain explicitly unobservable. See
[`decoded-delivery-memory-measurement`](../../../docs/decoded-delivery-memory-measurement.md).

Fatal startup teardown closes an initial Unix-socket client's cursor after the
terminal `Disconnect` position. The harness gives the writer a finite grace to
complete that frame, then shuts down an independently owned socket handle so
blocked read or write I/O wakes and the cursor retires. Complete kernel-queued
frames remain readable before EOF; a slow client may instead observe EOF, reset,
a partial frame, or no terminal reason. Generic stdio and pipe writers do not
inherit this socket-specific cancellation guarantee and retain synchronous
drain behavior where the harness requests a terminal drain.

Overall harness shutdown closes every in-process extension transport and gives
all runners one shared cleanup grace to return normally on EOF. A runner that
ignores closure, deadlocks, or blocks on unrelated work is detached when that
grace expires, rather than force-cancelled. Detachment lets shutdown return but
can retain resources in embedded or reusable hosts. The harness moves a runner
handle to detached join-reaper ownership, so it can record a completed join
without blocking shutdown on final Rust teardown; process-backed extensions
retain their separate supervised signal-and-reap cleanup. See
[SPEC-tau-harness-extension-lifecycle](SPEC-tau-harness-extension-lifecycle.md#overall-harness-shutdown).

Pinned foreground startup selects either exclusive exact-ID creation or strict
existing-state resume. Exclusive creation atomically claims only an absent
session directory and never reuses or repairs any existing directory shape;
strict resume retains the canonical manifest, journal, and lock validation.
Both modes install process-level SIGINT and SIGTERM handling
that wakes the central event loop. Coordinated shutdown retires listener
admission first, then shuts down harness connections and extensions, removes the
runtime socket and metadata, and returns normally to the supervisor. A second
SIGINT or SIGTERM restores that signal's default forced termination; it may
interrupt extension cleanup and runtime-file removal. Signal handlers perform no
semantic work themselves.

Harness storage policy is immutable for one process. Durable mode owns normal
session, agent, diagnostic, retention, and extension storage;
session-ephemeral mode suppresses only session-owned artifacts; memory-only
mode replaces both semantic stores with process-local stores and disables every
harness-managed persistent capability while preserving the ordinary event and
extension lifecycle. Only the unique runtime discovery pair may exist while a
memory-only daemon runs. Owned effective-prompt and tool-preview daemons combine
session-ephemeral mode with a process-local agent store so their one preview
agent cannot create durable state; provider and extension storage retain
ordinary session-ephemeral semantics. The system-prompt preview remains fully
memory-only. Normal durable and global session-ephemeral launches continue to
use the durable agent store.

The external-message, provider-model, provider-quota, provider-execution,
tool-lifecycle, tool-request, tool-progress, terminal-tool-outcome, user-shell-report, prompt-fragment,
session-discovery, per-agent-context, internal-prompt-request, start-agent-request,
terminal-output, custom-event, attached-UI-liveness, metadata-request, and
ambient-runtime-indicator slices
now use generic `Emit` publication, immutable authenticated
publisher snapshots, source-aware admission, and downstream processing as
required by
[SPEC-peer-event-publication](../../../specs/SPEC-peer-event-publication.md).
The metadata slice covers request-to-canonical publication; rejection outcomes
remain unspecified and preserve silent rejection.
UI extension-counter inspection now uses the dedicated
`ui_debug_event_stats_request` message with attached-socket-UI authority and a
directed, non-published notice result. UI detach now uses the dedicated
`ui_detach_request` message with the same authority and direct
connection-control behavior. Unconditional UI-requested shutdown uses the
payload-free `ui_shutdown_request` with the same attached-socket-UI authority;
it enters the same canonical lifecycle as signal or policy shutdown and does
not itself become an event. UI tree inspection uses `ui_tree_request` and
returns one requester-directed, non-published multiline notice. The
dedicated attached-UI request row is complete.
Agent creation accepts `ui.create_agent` only from attached socket UIs and
returns its terminal admission result directly to the initiating connection
without publication. Admission ends when durable creation plus initial-prompt
preprocessing acceptance produces `Created { initial_prompt: Queued }`.
Subsequent preprocessing and prompt execution use the separate correlated
prompt lifecycle; pre-materialization failures publish transient
`agent.prompt_failed` terminals. See
[SPEC-ui-create-agent-admission](../../../specs/SPEC-ui-create-agent-admission.md).
Configured extensions now use the dedicated `extension_notice_request`; the
harness creates a sanitized live-only `extension.notice` through ordinary
interception and broadcast. Extension-authored generic
`Emit(harness.notice)` remains denied. See
[SPEC-extension-notice-requests](../../../specs/SPEC-extension-notice-requests.md).
The general protocol-level authenticated publisher envelope and remaining peer
event families remain to be migrated.

Destructive cancellation after an interceptor request was delivered retains the
registration but suspends that connection from matching new publications until
exactly one stale reply is consumed without action. Registration replacement is
accepted during indefinite suspension; disconnect clears it and a new connection
starts unsuspended. See
[SPEC-tau-harness-event-processing](SPEC-tau-harness-event-processing.md).

Peer frames retain their admission session/generation through activation
staging. Rollover raw-commits documented session-bound observations but blocks
their downstream semantics at one shared generation boundary. Process-global
tool/prompt-fragment/model declarations and provider-quota current-state reports
survive rollover and run only under exact live connection/instance checks.

Per-agent context registration, correlated values, discovery snapshots, and
readiness commit before the harness updates runtime prompt projections or releases
waits. Exact connection-generation plus session/agent/initialization checks
prevent stale publishers or old load attempts from mutating current state under
[SPEC-per-agent-context-declarations-and-readiness](../../../specs/SPEC-per-agent-context-declarations-and-readiness.md).
Internal-prompt requests commit before loaded-agent validation or internal
prompt submission, and stale publisher generations cannot submit work. The
harness stamps accepted canonical prompt facts from the authenticated configured
publisher rather than request payload provenance. See
[SPEC-internal-prompt-submit-requests](../../../specs/SPEC-internal-prompt-submit-requests.md).
Start-agent requests likewise commit before role/parent validation, duplicate
route rebinding, acceptance/result routing, or child creation. See
[SPEC-start-agent-requests](../../../specs/SPEC-start-agent-requests.md).
Metadata set/unset requests commit before target and payload validation. Exact
live extension/UI checks prevent parked stale requests from publishing
harness-sourced durable canonical facts. See
[SPEC-agent-metadata-requests-and-canonical-facts](../../../specs/SPEC-agent-metadata-requests-and-canonical-facts.md).
Terminal-output events commit and broadcast before attached UIs act. They never
enter semantic replay, and replay-marked deliveries cannot repeat terminal side
effects. See
[SPEC-terminal-output-side-effect-events](../../../specs/SPEC-terminal-output-side-effect-events.md).
Custom extension-owned events likewise cross ordinary interception and commit
before direct live subscriber delivery. They have no harness semantic consumer
and never enter semantic replay. See
[SPEC-custom-extension-events](../../../specs/SPEC-custom-extension-events.md).
Prompt-draft and focus observations accept only attached socket UIs and cross
ordinary interception and commit before live subscribers react. They never enter
semantic replay. See
[SPEC-ui-prompt-draft-and-focus-events](../../../specs/SPEC-ui-prompt-draft-and-focus-events.md).

Architectural or externally meaningful functional changes to harness event
logs/journals or interfaces with extensions require the explicit confirmation
mandated by
[GATE-persistence-and-extension-interface-change-approval](../../../specs/GATE-persistence-and-extension-interface-change-approval.md).

Provider prompt materialization crosses the semantic-store and extension-route
boundary: a durable dispatch owner permits one content-free prompt-start append,
and only its write-complete post-commit continuation directs the transient full
request to the selected provider. The authority and crash cuts are governed by
[SPEC-provider-prompt-materialization-authority](../../../specs/SPEC-provider-prompt-materialization-authority.md).

The harness is the sole process-local owner of disabled-by-default Provider
cache refresh scheduling. It derives keyed exact-prefix identities and economic
eligibility only from current route policy, privacy, quota, price, and successful
read/write observations. It sends sensitive refresh and cancellation requests
only to the exact captured Provider route during a registered finite tool-batch
window, below real prompts, with bounded global and per-Provider admission.
Authenticated terminal reports release slots and derive content-free canonical
facts. Provider cooldown and real prompt priority suppress work. The owner does
not retry failure, persist lifecycle state, or restore it after restart. See
[SPEC-provider-cache-refresh-lifecycle](../../../specs/SPEC-provider-cache-refresh-lifecycle.md).

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
The harness owns runtime-only semantic work status for each loaded agent and
projects each current canonical phase/title through the complete transient
agent-stats snapshot as well as validated reports durably to current watchers.
The harness also owns the detailed transient turn-activity reducer. It combines
provider activity, prompt-frozen static tool categories, and the union of
configured Tool/Core ambient-indicator contributions without changing binary
runtime or outer-turn semantics.
Successful no-tool responses while Working, or while Unreported for a prompt
whose frozen tool surface exposes `status`, remain durable assistant transcript
entries but withhold watch, worker-result, and detach projections for at most two
challenges in each unresolved phase. Entering Working resets the budget even
after Unreported challenges in the same outer turn. Waiting, Done, or Blocked permits the
next successful final to project; otherwise the third successful final within
the current unresolved phase escapes the guard. Working becomes Unknown on
escape, while Unreported remains unchanged. Agents without `status` remain
unaffected. Every accepted final projects only after its exact append commits.
Unsuccessful terminals invalidate Working to Unknown without a challenge.
Activating-input wait timeouts remain successful scheduling results. The third
consecutive timeout without a status report or substantive tool admission adds
one bounded harness-authored advisory; later timeouts in that no-progress run
remain unadorned, and current Waiting suppresses the advisory entirely.

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
processing and publishing protected harness-sourced terminal or provider facts.
Successful provider and background facts then produce distinct payload-free UI
display events in both live and replay paths; see
[SPEC-terminal-tool-reports-and-canonical-outcomes](../../../specs/SPEC-terminal-tool-reports-and-canonical-outcomes.md).
Ownerless and harness-internal foreground errors use the same provider-first
post-commit reaction without acquiring journal ownership: their runtime
`provider.tool_error` commit precedes `tool.error`, wait/loop settlement,
accounting, and teardown cleanup.
Configured Tool/Core shell providers likewise submit
`shell.command_progress_reported` and `shell.command_finished_reported` through
generic publication. The post-commit consumer revalidates the captured
generation, frame-admission session, and private routed command before publishing
harness-sourced canonical progress/completion; transcript injection follows
canonical completion commit. Immutable original-route classification and
process-lifetime harness-route tombstones keep ephemeral report payloads out of
durable debug JSONL across interception and session rollover; unknown
peer-chosen routes retain ordinary audit treatment. See
[SPEC-shell-command-reports-and-canonical-facts](../../../specs/SPEC-shell-command-reports-and-canonical-facts.md).
Configured Provider/Tool/Core peers submit `tool.request` through generic
publication before routing. The post-commit consumer revalidates the captured
generation and call-id correlation, installs terminal ownership, and publishes
harness-sourced started or rejection/terminal facts. Caller-selected durable
requests retain stable configured publisher provenance but never rerun work on
replay; see
[SPEC-tool-requests-and-routing](../../../specs/SPEC-tool-requests-and-routing.md).

Debug JSONL has a deliberately separate runtime/I/O boundary. Harness event
handling redacts and serializes eligible observations, then attempts immediate
nonblocking admission to one lazy process-lifetime bounded FIFO. One detached
worker owns every append handle and performs directory/open work, per-line
`events.jsonl.lock` acquisition, exact-EOF append, flush, and rollback. Queue
accounting includes queued and in-flight line-plus-path bytes; overflow and
recoverable I/O omit diagnostics, while uncertain rollback poisons the singleton.
No harness lifecycle owns, drains, joins, or fsyncs this worker. Authoritative
CBOR journals use a separate bounded, non-droppable ordered persistence queue
and worker under
[SPEC-semantic-journal-writeback-durability](../../../specs/SPEC-semantic-journal-writeback-durability.md).
Startup separately runs one best-effort, time-based cleanup of expired session
`events.jsonl` files and exact compressed provider request/response or
failed-attempt diagnostic
captures, defaulting to fourteen days while excluding current or locked
sessions, symlinks, unrelated diagnostics, and all canonical journals.

Opaque Provider debug captures arrive through a dedicated non-event protocol
message with Provider-supplied session/prompt attribution. The harness accepts
them only from the current authenticated Provider connection and a known
durable session, derives the configured-instance path itself, and queues exact
zstd bytes on a bounded filesystem worker without parsing or decompression.
Late captures retain their attributed old session across rollover; missing
session roots, overload, failures, and shutdown omit the best-effort artifact.
The writer creates capture directories with owner-only access and capture files
with owner read/write access, tightening existing capture-directory modes before
each write.

Live provider retry updates may carry bounded provider detail, but the
non-authoritative `events.jsonl` projection replaces it with only retry
category, attempt, delay, and retrying status. Watcher and agent-message
projections continue to derive solely from the closed retry fields.

Configured Provider execution uses the same generic commit boundary. Five `_reported`
observations commit before exact generation and prompt/retry correlation; the harness
then publishes canonical provider facts or a requester-directed retry outcome. Terminal
response alternatives retain the existing recovery, persistence, tool dispatch, and turn
closure pipeline. See
[SPEC-provider-execution-reports-and-canonical-facts](../../../specs/SPEC-provider-execution-reports-and-canonical-facts.md).
Standalone backend terminals publish their canonical accounting independently;
only its committed facts update live ledgers. Restore folds those same facts only
when their required session identity matches the session being rebuilt. It scans
unloaded durable journals through bounded read-only snapshots rather than
retaining exclusive session leases. Sequence-continuing Initial/New bindings
restore that ledger without recreating the old runtime branch. Post-dispatch
cancellation publishes one Unknown awaiting observation; only a terminal from
the same live provider generation may correct it without counting another
request. Provider loss, shutdown, and unload close remaining owners as Final
Unknown before runtime authority disappears.
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

The post-commit prompt consumer validates universal fields and immediately
creates one payload-free live wake for valid incoming facts. `message.sent`
becomes assistant context without activation. The sole tree-global foreground
tool round defers only branch-applicable transcript placement and provider
dispatch; it does not defer wake creation. Root, ancestor-above-assistant, and
sibling facts materialize immediately. The fact itself always broadcasts
immediately. Replay reconstructs the same branch-applicable context but never
creates a runtime wake, resends transport traffic, or rebuilds
extension-private authority. Invalid or unavailable targets remain
committed and visible to subscribers even when no prompt projection is possible.
The complete schema, persistence, and projection contract is
[SPEC-external-message-reports-and-facts](../../../specs/SPEC-external-message-reports-and-facts.md).
Agent-message and shared tool-round placement is specified by
[SPEC-agent-message-delivery](../../../specs/SPEC-agent-message-delivery.md).

## Provider model declarations and canonical state

Only authenticated configured provider extensions may publish transient,
interceptable `provider.models_declared` replacement declarations. Providers
declare routes only after local credential hydration and replace the declaration
when a later credential observation changes; this state makes no
remote-authentication guarantee. The generic
publication envelope snapshots the configured connection and provider kind so
parking, disconnect, or replacement cannot substitute publisher identity.
Extension-supplied persistence requests are ignored categorically.
Post-commit processing stages startup declarations until activation or publishes
protected harness-authored `provider.models_updated` current state before applying
the existing route, collision, availability, and restored-work reconciliation.
At that boundary the harness validates declaration entries independently, emits
one protected structured diagnostic for each rejected entry, and carries valid
siblings into the canonical replacement snapshot unchanged. Invalid metadata is
neither repaired nor allowed to suppress unrelated valid routes.
Canonical model state cannot be dropped or rewritten; the existing availability
projections retain their existing interception behavior. Each canonical snapshot
also carries the stable configured provider publisher so replacement and empty
snapshots remain attributable even though their delivery source is the harness.
Subscribe-time current-state catch-up synthesizes canonical updates with that stable
publisher and harness source metadata only. It is not durable replay, never
regenerates declarations, and never reruns their side effects. Session rollover
retains a deferred declaration and applies it
when the captured connection/configured instance remains exact because model state
is process-global. The payload and event-name contract is documented in
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

The harness also owns bounded, redacted peer and local-agent discovery
snapshots. Runtime metadata advertises only an untrusted entrypoint hint; the
live harness confirms its current session and effective policy through a narrow
probe.
The same event loop owns inter-session receiver admission, fair live selection,
and configured-order role auto-start. It admits bounded count/bytes/rate before
creation, treats pending and busy eligible agents as reusable endpoints, and
releases sender success only from the receive projection's post-commit
continuation. This state is generation-bound and in-memory; crash ambiguity
follows best-effort at-least-once semantics.
The committed directional facts, sequence-aware semantic placement, runtime-only
recipient wake, branch acknowledgement, canonical provider rendering, and
replay boundary are specified across harness and core by
[SPEC-agent-message-delivery](../../../specs/SPEC-agent-message-delivery.md).
The peer-created endpoint purpose itself is ordinary durable lifecycle state: the
harness embeds a reserved, non-inheritable metadata marker in the immutable
ordered `AgentStarted` creation fact and restores it before extension-query
teardown classification. Interception cannot drop or rewrite the protected
creation fact, and general metadata intake cannot set, unset, or inherit this key.

## Watch ownership

The harness owns the live acyclic topology, endpoint retirement, sanitized provider-work snapshots, and notification fanout specified by [SPEC-agent-watch](../../../specs/SPEC-agent-watch.md) and [GATE-agent-watch-acyclic-topology](../../../specs/GATE-agent-watch-acyclic-topology.md). Display labels remain separate from topology.


## Skills

The harness atomically replaces each extension connection's complete discovery
source, validates bounded skill/AGENTS.md items, and resolves stable collision
slots. Session winners drive role preflight and agentless UI completion. Every
agent initialization starts from that baseline and freezes its own finalized
skill/bootstrap state.

User `:skill <name> [args]` and `:skill:<name> [args]` expansion uses the selected
agent's frozen snapshot. New-agent initial commands defer expansion until
finalization. The model skill tool and `<available_skills>` use the same frozen
state. Unknown, invalid, unreadable, or non-user-invocable commands emit a notice
and are not submitted.

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
Session registration, complete source snapshots, and readiness cross ordinary
interception and commit before they atomically update projections or release the
barrier. Finalization stores one durable replaceable bootstrap side-state fact;
it does not append ordinary AGENTS.md transcript messages.
See
[SPEC-session-discovery-declarations-and-readiness](../../../specs/SPEC-session-discovery-declarations-and-readiness.md).

## Daemon and provider reliability boundaries

Harness startup accepts one complete normalized `HarnessSettings` snapshot before
any configured process launch. Extension resolution, provider and secret inputs,
roles, tool policy, retention, and prompt settings derive from that snapshot, and
runtime baseline lookups retain it instead of rereading configuration. Missing
optional user configuration remains a valid built-in snapshot; an existing
unreadable or invalid layer fails startup with context. This is not a live-reload
mechanism: an already-running harness keeps its accepted snapshot.

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
cannot be made atomic with PID reuse and listener replacement. Before the
interactive UI owns daemon disposition, CLI error cleanup closes the initial
transport and gives exit-on-disconnect a bounded grace to remove the runtime
pair, with forced termination only as fallback. Once the UI runs, every UI exit
leaves child lifetime to exit-on-disconnect, detach policy, or an explicit
canonical shutdown request; it does not directly force-terminate the child.
Targeted session lookup
may traverse a larger bounded raw catalog than general peer discovery so stale
unrelated pairs do not consume the much smaller matching-candidate budget.
Local running-session listing isolates bounded runtime-path traversal, then uses
a per-candidate, correlation-matched local socket RPC to obtain each responsive harness's
in-memory current session id and immutable canonical startup project root.
Runtime metadata and persisted session directories provide neither live records
nor returned field authority. The overall scan has a fixed deadline and fails
instead of returning a partial snapshot when candidate traversal or the total
probe budget is incomplete.
The wire contract is specified by
[SPEC-tau-proto-session-events](../../tau-proto/specs/SPEC-tau-proto-session-events.md).

Accept-loop shutdown must use an owned wake/cancellation primitive tied to the
accept thread, not polling sleeps and not the filesystem socket pathname. Runtime
socket paths can be removed or replaced while a cloned listener fd remains live, so
shutdown correctness must not depend on reconnecting to that path. Internal wake
traffic is control-plane state only and must never be forwarded as a harness
client.

The harness validates provider prompt ownership and derives public routing identity, but providers retain streaming and response-throughput authority under [SPEC-provider-response-streaming](../../../specs/SPEC-provider-response-streaming.md). Public stats are content-free and transient; they never become transcript, editor, prompt-stdin, or final-response content.

The harness owns an active-session `AgentCreatorTopology` and separate
`AgentCostLedger`. Topology accepts only authenticated same-session
`AgentStarted.creator = AgentCreator::Agent` edges, preserves the first valid
edge, and retains relationships through individual runtime retirement.
`parent_agent`, remote provenance, and legacy missing creators contribute no
edge. Accepted response usage increments self cost and every creator ancestor's
inclusive subtree cost with saturation; costs reset at rollover while resume
re-seeds only loaded creation edges. Complete transient stats snapshots update
the changed agent and each loaded creator ancestor.

## Agent navigation authority

Current-session runtime owns loaded-agent navigation modes alongside membership
and routing. Modes affect UI eligibility only, never loading, routing, delivery,
watches, execution, or model behavior.

The event loop applies both authenticated explicit UI writes and the implicit
`active` write caused by admitting a visible human prompt to an existing loaded
target. The accepted-interaction append precedes the mode/stats publication,
which precedes queue or dispatch. Complete harness stats are the only UI
projection authority; CLIs and transcript replay must not infer a mode change.
The authenticated bare peer-entrypoint auto-start path applies the same
runtime-only mode/stat flow only to its newly created recipient after durable
creation and current-session membership setup. Existing recipients and other
start paths retain their ordinary navigation defaults.

The wire contract is specified by
[SPEC-tau-proto-session-events](../../tau-proto/specs/SPEC-tau-proto-session-events.md).

## Directed agent roster

The event loop owns a bounded, read-only current-session roster RPC for local UI
connections. It reads current and ever-loaded caches atomically seeded from
validated committed membership before runtime restoration and updated only after
later membership commits. Restore/commit failures invalidate the projection.
The RPC checks the entry limit before cloning ids, joins live runtime/navigation
state, then adds shallow bounded creation facts.
Live rows also copy phase/title and detailed turn activity directly from each
agent's harness-owned runtime projection.
Results are correlated and requester-directed; they are not events and never
enter persistence, interception, publication, subscription replay, or extension
delivery. Exact wire behavior is specified by
[SPEC-tau-proto-session-events](../../tau-proto/specs/SPEC-tau-proto-session-events.md).
The harness owns configured-instance Secret storage, payload-opaque RPC routing,
and the mandatory outer Linux namespace launcher. It masks the whole secret root,
mounts an ephemeral read-only materialization of the merged config/state provider
snapshot only for configured Provider instances, and fails supervised startup
closed. Every configured Provider gets the same immutable credential-free
`Configure.settings_files` snapshot; a memory-only
preview gets that snapshot without the providers mount, while Tool
instances receive neither surface, as specified by
[SPEC-extension-secret-storage](../../../specs/SPEC-extension-secret-storage.md).
For built-in Providers, startup locks each mutable instance directory, captures
one bounded disjoint-union generation from XDG config and state, rejects every
cross-layer duplicate, and resolves valid named API-key bindings without
forwarding those declaration values in Configure, publishes typed records under
the second-level Secret lock, and retains the same generation for
`Configure.settings_files`. A typed component identity survives argv wrapping
and prevents external replacement commands from gaining this authority. Actual
absence suppresses stale credentials; source errors preserve the previous record
and follow required/optional extension startup policy. Memory-only startup
forwards the credential-free settings generation needed for model declarations
without resolving or materializing credentials.
