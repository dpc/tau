# Security policy

The disabled-by-default `std-rostra` extension is a trusted same-user
executable, but every synchronized Rostra field is untrusted external content.
Rostra signatures authenticate an author key, not instructions or Tau
authority. It derives one identity from a declared Tau-managed mnemonic secret,
keeps that signing authority in its trusted same-user process after lazy first
write activation, and exposes bounded reads plus explicitly signed social
writes. The extension uses relay-only Iroh peer transport plus Pkarr HTTPS/DNS
discovery without direct peer-IP transport, owns an exclusive owner-private
per-instance redb containing public graph data, signed local events, and an
Iroh node secret. Signed effects are locally durable but remotely asynchronous;
unknown timeout/cancellation outcomes can have taken effect. Its only inbound
model-visible path is the separately opted-in, bounded following-notification
report: it projects hostile Rostra content into an extension-owned checkpointed
batch and has no arbitrary inbound message or reply route. It has no identity
creation, direct-IP public mode, shared database, or memory-only fallback. See
[`ARCH-tau-ext-rostra`](crates/tau-ext-rostra/specs/ARCH-tau-ext-rostra.md).

The optional `std-swarm` configured extension is a trusted same-user local
executable. Its remote Iroh peer is cooperative but authenticated and
identity-pinned before the worker credential is sent; externally supplied
prompts, answers, identifiers, and collections remain size-validated inputs.


Tau is early-stage software, but security issues are important. Please report suspected vulnerabilities through GitHub private vulnerability reporting for `dpc/tau` (<https://github.com/dpc/tau/security/advisories/new>) when available. If that path is unavailable, contact the maintainer privately first and avoid filing a public issue with exploit details.

For technical trust boundaries, start with [ARCH-external-message-boundary](specs/ARCH-external-message-boundary.md) and the applicable project and component records under `specs/` and `crates/*/specs/`.
Supervised extension state isolation is defense in depth inside the configured
same-user executable boundary, not hostile-code containment. `hidden` and
`read_only` prevent ordinary discovery or mutation of unrelated Tau state while
preserving exact extension-owned state; they do
not defend against procfs, ptrace, pre-opened descriptors, unrelated host data,
or authorized secret RPC delivery.
The launcher covers its temporary real-state staging tree with an empty
read-only mount before exec, after installing the exact destination binds, so
the child cannot bypass the selected view through the staging source path.

Provider debug captures cross the cooperative configured-extension protocol as
dedicated non-journaled messages. The Provider zstd-compresses opaque artifacts
off its request path and supplies only typed session/prompt attribution; the
harness authenticates the Provider connection, derives the instance-specific
durable-session path, and writes without parsing or decompressing payload bytes.
Both transport and filesystem queues are bounded and best-effort. Capture
payloads never enter events, journals, debug JSONL, or generic Debug output.
Disabled-by-default cache refreshes resend an exact previously successful
Provider-visible prefix. The harness keeps that content in process memory and
sends refresh/cancel requests point-to-point only to the captured configured
Provider generation. Sensitive requests are excluded from broadcast,
interception, journals, replay, debug JSONL, generic Debug output, watchers, and
UI projection. Content-free terminal reports and canonical facts may remain
observable. Provider-specific private request capture can still contain the
upstream refresh request when the operator separately enables that capture.
The distinct deterministic and live/VCR test-fixture boundaries are documented
in [`tau-e2e-tests/SECURITY.md`](crates/tau-e2e-tests/SECURITY.md).
The disabled-by-default test-dummy extension's capability and worker-lifecycle
boundary is documented in
[`tau-ext-test-dummy/SECURITY.md`](crates/tau-ext-test-dummy/SECURITY.md).
Authenticated ChatGPT quota acquisition and its credential-free lifecycle are
documented in
[`tau-provider-codex/SECURITY.md`](crates/tau-provider-codex/SECURITY.md)
and
[`tau-ext-provider-builtin/SECURITY.md`](crates/tau-ext-provider-builtin/SECURITY.md).
The same component records cover the split provider trust boundary: generic
OpenAI-compatible Chat Completions uses HTTP/SSE, private ChatGPT/Codex inference
is WebSocket-only, and all built-in provider egress shares the immutable policy
documented in
[`tau-provider/SECURITY.md`](crates/tau-provider/SECURITY.md).
The generic public Responses parser's validated reasoning, sensitive replay
sidecar, and configured-endpoint boundary are documented in
[`tau-provider-responses/SECURITY.md`](crates/tau-provider-responses/SECURITY.md).

## Terminal presentation boundary

Tool, provider, extension, and user-derived presentation text is not trusted
terminal control. Themes resolve to structured styles rather than inline escape
bytes, and terminal cell conversion sanitizes control characters before output.
Adaptive one-row layout measures complete Unicode graphemes by terminal display
columns and middle-truncates only at grapheme boundaries. Tool headers treat
identity and explicit lifecycle/result status as one essential set: a terminal
too narrow for that set shows neither the header nor its owned payload/diff body
rather than exposing anonymous details or obscuring success versus failure.
Re-check control sanitization, zero- and multi-column graphemes, exact
minimum/maximum boundaries, tiny widths, owned-body suppression, and resize
restoration whenever terminal layout or tool presentation changes. See
[`ARCH-tau-term-screen`](crates/tau-term-screen/specs/ARCH-tau-term-screen.md)
and [`ARCH-tau-cli`](crates/tau-cli/specs/ARCH-tau-cli.md).
The harness alone stamps prompt provenance at the configured-extension boundary:
an accepted internal-prompt request produces one canonical prompt fact with the
authenticated configured extension name, never a request-supplied name. CLI
attribution escapes that bounded name as visible metadata and does not create a
second transport message or model activation.
The built-in Swarm `blocker` tool has a narrower presentation boundary: the
CLI permits only its finite `add`, `cancel`, or `list` action discriminant from
the start arguments, and strips all other start, progress, and terminal display
fields in compact and full modes. Missing, malformed, or unknown actions fail
closed. Re-check valid and invalid actions across live, progress, result, error,
cancellation, replay, and cold attachment whenever blocker presentation changes.

Interactive frontend progress diagnostics contain only process-local delivery
ids, typed event names, agent routing ids, selected/hidden classification,
queue item/encoded-byte counts and ages, stage durations, output block counts,
and cancel target resolution. They never retain prompt, response, tool, event,
terminal-output, or disconnect-reason bodies. Trace-level paired stage markers
localize an operation that never returns; warnings for completed stages at or
above 500 ms are rate-limited per frontend component to one per five seconds.
The CLI socket-to-renderer FIFO is bounded at 1,024 items and 64 MiB, but
backpressure can migrate backlog to the harness writer queue. These bounds
therefore do not promise whole-process or end-to-end slow-client memory limits.

## Command-mode and prompt boundary

First-non-whitespace `:` selects non-provider command authority. Unknown or
malformed colon commands fail locally, while slash-prefixed text—including
obsolete command spellings—is ordinary provider input. A doubled `::` escape
keeps a typed literal marker through every harness-owned command consumer even
though history, durable prompt text, and provider projection contain only the
canonical single-colon text. This prevents canonical `:skill` prompt text from
being reinterpreted after the CLI removes the escape.

Only attached socket UIs may send `ui.create_agent`. The harness returns its
bounded, sanitized admission result directly to that live connection without
publication or replay. Distinct bounded request and prompt correlation ids keep
creation admission separate from later prompt processing. Pre-materialization
prompt failures publish only bounded sanitized diagnostics and correlation
metadata as transient `agent.prompt_failed` terminals; canonical provider
failures retain their existing prompt-id lifecycle. See
[SPEC-ui-create-agent-admission](specs/SPEC-ui-create-agent-admission.md).

Gmail OAuth finish arguments remain raw only for exact-owner extension routing.
The CLI redacts them from command echo and persistent prompt history, and the
harness excludes transient inbound invokes from debug JSONL and redacts the
published debug-log copy. Re-check stale
user-facing command instructions, interactive/headless parity, completion
precedence, literal escape handling around skills, and both debug-log paths
whenever command routing or action logging changes. See
[GATE-colon-command-mode](specs/GATE-colon-command-mode.md),
[SPEC-tau-cli-command-mode](crates/tau-cli/specs/SPEC-tau-cli-command-mode.md),
and
[SPEC-tau-harness-session-state](crates/tau-harness/specs/SPEC-tau-harness-session-state.md).

Peer harness messaging is cooperative same-UID local IPC, not a hostile-process
sandbox or per-sender ACL. Callback correlation prevents accidental sender/route
confusion before bounded admission or model-spending auto-start, while peer text
remains model input rather than a harness instruction. Delivery is best-effort
at-least-once: an ambiguous crash or retry can duplicate receive occurrences,
agents, model work, and spend. Each accepted directional occurrence is its
owning journal's sole canonical payload projection. Local inbound provider
context exact-close-frames peer text inside a sender-labelled wrapper; live activation
uses a payload-free sequence wake, and replay restores context without waking.
The target's complete foreground framed write remains ACK authority; ACK does
not wait for background filesystem sync. An ACK or provider effect can therefore
survive a crash that loses its journal fact. Tau adds no restart deduplication,
distributed WAL, or cross-journal transaction. See
[SPEC-semantic-journal-writeback-durability](specs/SPEC-semantic-journal-writeback-durability.md).

## Agent journals and summary checkpoints

The three `tau dev print-*` render previews use an immutable memory-only
harness policy: they may read render inputs but do not create, inspect, repair,
or mutate harness-managed session, agent, diagnostic, retention, or delegated
extension storage. Only their unique runtime socket and discovery metadata may
exist while the owned daemon runs; handled exits remove that pair after child
reap. Configured extensions remain trusted same-user executables and
unsandboxed, so their direct operating-system side effects are outside this
guarantee.

Per-agent `events.cbor` journals are authoritative durable identity and
transcript state. Their `meta.json` files are content-minimized, atomically
replaced derived checkpoints, not routing authority. Checkpoints bind an exact
frame boundary and sequence to journal file identity and a boundary witness;
stale or invalid checkpoints are repaired only under nonblocking byte, record,
and time budgets. Metadata-only, empty, corrupt, or otherwise unvalidated
artifacts reserve ids but cannot receive routed facts.

Agent `events.cbor` and session `events.cbor`/`restore-events.cbor` use
length-prefixed CBOR frames. Prefix or payload-write failure triggers truncation
to the exact pre-append EOF before the caller receives the original append
error. Only failure to restore that EOF poisons the journal path in the live
store. On reopen, a locked writer truncates only an incomplete frame header or
payload at EOF, which represents a crash tail. A complete frame that fails typed
decode, source-shape validation, sequence validation, or semantic validation
fails closed without changing that frame or any following bytes. A valid
complete frame immediately advances folded state and sequence.

A lifecycle-owned worker coalesces dirty journals and required directory
coverage, syncs in the background, tracks generations to avoid lost wakes, and
retries failures without retracting accepted facts. Locked recovery truncates
only an incomplete EOF crash tail and invalidates derived metadata after that
repair. Complete invalid frames fail closed without mutation. Re-check byte-boundary, rollback-failure,
retry-sequence, restore-journal, writeback, and suffix regressions whenever
framed I/O changes.

Provider work leaves the harness only after its inference or compaction owner
and one matching source-free `agent.prompt_started` frame have completed
foreground semantic appends. Delivery does not wait for background sync, so a
crash can preserve provider effects while losing journal facts. The fact's
one-shot continuation rechecks the session,
loaded runtime incarnation, unresolved owner, exact identity, and captured route
before directing the transient full prompt; an owner-only or owner-plus-start crash
cut never resends work. Persisted old full prompts are unsupported. Debug JSONL
represents full prompts only as bounded content-free summaries. Re-check these cuts
whenever prompt persistence, interception, replay, routing, or diagnostics change;
see
[SPEC-compact-prompt-materialization-authority](specs/SPEC-compact-prompt-materialization-authority.md)
and
[SPEC-tau-harness-prompt-dispatch](crates/tau-harness/specs/SPEC-tau-harness-prompt-dispatch.md).

Session debug `events.jsonl` producers redact and serialize a complete line,
then attempt immediate nonblocking admission to one lazy process-wide FIFO
bounded at 1,024 retained lines and 64 MiB of line-plus-path bytes including
in-flight work. The detached worker holds `<session>/events.jsonl.lock` for each
line through handle selection/open, exact-EOF append, flush, and rollback.
Overflow and recoverable lock/open/write failures omit rows; uncertain rollback
globally poisons the worker. The worker never fsyncs. No session/process shutdown
path requests or waits for a drain or joins the worker; it may continue draining
while the process remains alive, and exit may interrupt queued or in-flight work.
This mirror is an ordered best-effort subsequence, not
authoritative evidence: absence never proves an event was absent, termination
can lose queued/OS-cached rows or tear the final line, and restart neither
repairs nor salvages it. Re-check bounds under held locks, path switching,
per-line lock reacquisition, overflow recovery, I/O retry, global poison,
warning coalescing, and nonjoining exit whenever debug-log I/O changes.
Startup cleanup applies the configured time window only to unlocked session
`events.jsonl` regular files and exact legacy `.json` or compressed `.json.zst`
provider request/response captures. It does not follow symlinks or remove
canonical agent/session journals, unrelated debug files, or extension-owned
JSONL.

Summary files intentionally omit prompt previews. Legacy preview-bearing
sidecars are unverified hints and are scrubbed when strict journal migration can
acquire the agent lock. Bounded checkpoint repair never rewrites journal facts.
Writer recovery may truncate only an incomplete EOF crash tail; complete invalid
frames and their suffix remain unchanged and fail closed.
Failure to publish derived metadata does not invalidate an already committed record.
This is cooperative same-UID crash consistency, not tamper detection: arbitrary
same-inode/same-size journal mutation is outside the append-only store contract.

## Prompt-history persistence

Global prompt history uses a bounded best-effort FIFO worker. Its queue has 64
pending slots, and its queued plus in-flight prompt text is capped at 1 MiB;
the worker can hold one of those accepted entries in flight while it writes.
The newest entry drops when either bound is full or the worker is unavailable.
Admission does not mean the record reached the filesystem or is durable. The
worker does not flush, fsync, or drain at shutdown, so a crash or normal exit
can lose accepted history.

The worker retains cooperative cross-process locking and repair. A
process-local device/inode/EOF/final-boundary witness avoids rescanning an
unchanged framed prefix under the cross-process lock and falls back on ordinary
replacement, truncation, or tail mismatch. The witness is not cryptographic
tamper evidence; a same-UID process deliberately preserving its identity and
witnessed tail while rewriting older bytes remains outside the prompt-history
contract.

Cold restore detaches completed start-agent workers only from validated journal
evidence that matches warm side-conversation terminalization. Explicitly
continuing recovery is not completion; terminal compaction failures are
completion when their originating side request matches. Histories without
unambiguous terminal evidence retain prior behavior and do not recover transient
result routes. Re-check this classifier and its two-boot positive/negative
regressions whenever provider-response, compaction, or side-conversation
terminalization changes.

## Agent display names

Agent display/task names are presentation-only metadata, never routing or trust
identity and never provider context or semantic message content. Local names are
authoritative only within the session whose agent metadata supplied them; remote
names are eligible only when carried by a typed peer endpoint. Message labels
keep the stable id visible, escape controls, bidi/structural Unicode, delimiter
characters, quotes, and backslashes, then apply byte and terminal-column bounds
after escaping. Revisit these invariants whenever adding a new name source or
using names outside UI presentation.
For newly created agents, the built-in default leaves agents without an
explicit task or rename unnamed. Operator-configured templates may still
generate names, and durable display-name facts remain authoritative on replay
even when an older generated name is indistinguishable from an explicit one.
See
[`SPEC-tau-cli-agent-message-labels`](crates/tau-cli/specs/SPEC-tau-cli-agent-message-labels.md)
for the rendering and session-provenance behavior.

## Agent-watch topology

The harness accepts only acyclic current-session agent-watch topology to prevent
reciprocal or longer feedback paths from amplifying watch-derived interactions.
For each genuinely new enable, Live-target validation and iterative reverse-path
rejection occur before any watch state or event changes; the check and mutation
share one synchronous, exclusively mutable harness event-loop operation.
Repeated enables preserve their existing edge, while disables remain available
to remove relations and bypass cycle analysis. Re-check this ordering and the
no-mutation failure contract whenever watch topology ownership or event-loop
serialization changes. See
[`GATE-agent-watch-acyclic-topology`](specs/GATE-agent-watch-acyclic-topology.md).

Work-status titles are model-authored cross-agent content, not trusted routing
or instruction data. The harness keeps their typed phase and epoch separate and
applies visible trusted-frame escaping before prompt interpolation, while the
CLI applies the same escaping before placing titles in its one-line status
frame. Long-wait
notifications contain only harness-derived numeric thresholds. Their scheduler
uses actual monotonic installed-wait intervals, advances even without watchers,
and never reconstructs runtime clocks or re-fans committed thresholds on replay.
Late watchers receive no historical threshold activation. Overdue catch-up
captures crossed thresholds and their current subscriptions compactly, advances
the timer cursor before later watch changes, and materializes at most 64 recipient
occurrences per runtime scheduler cycle. A queued event runs between batches;
without one, the remaining compact backlog schedules another immediate cycle.
Only the model-owned, policy-authorized `status` call may mutate its calling
agent. Configured extensions cannot invoke it directly or select a target agent.
The harness validates the closed phase and canonical 160-byte, single-line title
at both tool and durable boundaries. A challenged successful response becomes
watchable or completes delegated work only after its semantic append and bounded
challenge lifecycle; append failure, interception rejection, unload, and session
rollover release that runtime ownership. Revisit these invariants whenever
internal-tool ownership, response interception, or agent teardown changes.

## Prompt capability authority

The harness's immutable post-policy, provider-filtered prompt snapshot is the
sole authority for tool definitions, call authorization, and capability-derived
prompt guidance. Revoking `agent_start` must remove both available-role catalog
text and its fragment entry from custom-template data; conditionally empty
ordinary fragments are therefore omitted after rendering. Re-check capability
filtering and custom-template regressions whenever prompt fragment projection or
effective tool policy changes. See
[`SPEC-tau-harness-prompt-dispatch`](crates/tau-harness/specs/SPEC-tau-harness-prompt-dispatch.md).

Harness-stamped prompt provenance is also provider-presentation authority.
Only `PromptSubmissionSource::HumanUi` receives the fieldless `<user>` envelope,
and only exact `</user>` collisions are replaced before the trusted close is
appended. The body remains the authenticated user's instruction
channel, not external untrusted-message metadata. Canonical journal facts,
UI/history/navigation, and watch fanout retain raw accepted text; replay derives
the same provider form from the typed source and never infers provenance from
text. Only exact outer Tau-stamped sentinels establish model-facing provenance;
nested or delimiter-like payload text does not change enclosing source or trust.
This prevents exact lexical breakout, not semantic prompt injection. Re-check
submitted/steered source preservation, exact-close replacement,
compaction suffix handling, and non-HumanUi exclusions whenever prompt folding
or provider assembly changes. See
[`SPEC-interactive-user-prompt-envelope`](specs/SPEC-interactive-user-prompt-envelope.md).

## Local IPC and external ingress

Configured extensions, including Providers, are trusted same-user executables
under
[GATE-configured-extension-trust-boundary](specs/GATE-configured-extension-trust-boundary.md).
“Less-trusted extension” means protocol authority is limited—the harness still
validates phase, source ownership, routing identity, configuration, and
collisions—not that the stdio stream is a hostile availability boundary or
process sandbox. Operation quotas do not promise to bound protocol
deserialization; see
[`SPEC-tau-harness-session-state`](crates/tau-harness/specs/SPEC-tau-harness-session-state.md#extension-data)
and [`ARCH-tau-supervisor`](crates/tau-supervisor/specs/ARCH-tau-supervisor.md#child-environment).
Every configured extension kind may request per-agent metadata mutations, while
attached socket UIs have the same narrow authority. Requests commit before
target/key/value validation, then the exact still-live extension generation or
attached UI is revalidated before the harness publishes a durable canonical
fact. Other socket peers and peer-authored canonical facts are rejected. Metadata
is extension-visible coordination state, not a secret store; invalid requests
currently have no outcome event. See
[`SPEC-agent-metadata-requests-and-canonical-facts`](specs/SPEC-agent-metadata-requests-and-canonical-facts.md).
Only an attached socket UI may send `ui_debug_event_stats_request` to inspect
configured-extension protocol counters. Dedicated external-message peers,
non-UI sockets, and embedded/non-socket UIs receive only a content-free
authorization error. The request is omitted from debug JSONL and its result is a
requester-directed, non-published notice. Configured extensions are silently denied
without a response, warning, or disconnection.
Every authenticated configured extension kind may send
`extension_notice_request`, but the request carries no kind, target, transience,
visibility, publisher, correlation, or provenance authority. The harness caps
critical to warning and publishes a harness-sourced, live-only
`extension.notice` through ordinary interception. Unconfigured and disconnected
origins are silently denied, and generic extension `Emit(harness.notice)` remains
forbidden. Pre-Ready requests retain normal activation ordering and bounded
admission. `ConfigError` remains a separate mandatory replayable path. Security
review must preserve those distinctions and the non-persistence guarantee. See
[`SPEC-extension-notice-requests`](specs/SPEC-extension-notice-requests.md).
Only an attached socket UI may send the payload-free `ui_detach_request` that
keeps the daemon alive after that UI disconnects. Other socket peers,
embedded/non-socket UIs, dedicated external-message peers, and configured
extensions are silently denied; they cannot mutate the daemon's
exit-on-disconnect control. Configured extension attempts retain normal protocol
phase validation and metering but are denied before activation staging.
Only an attached socket UI may send `ui_tree_request` and inspect agent prompt
anchors/previews. The harness returns exactly one requester-directed multiline
notice and does not publish the request or result. Other client origins and
configured extensions are silently denied; extension attempts retain normal
phase validation and metering but are denied before activation staging.
Only authenticated configured Tool/Core peers may publish transient tool
registration/unregistration declarations; canonical `tool.register` and
`tool.unregister` state is harness-authored. After declaration commit, the
harness binds processing to the captured configured identity and live connection,
then enforces assigned prefixes, schema/example bounds, ownership, and startup
collision policy. Pre-Ready reservations are bounded and released on
drop/disconnect, and neither declarations nor canonical runtime state enters
semantic journals. Security review must revisit this boundary when changing the
authority matrix, interception replacement/drop behavior, activation accounting,
disconnect/respawn identity checks, or persistence classification. See
[`SPEC-tool-declarations-and-canonical-state`](specs/SPEC-tool-declarations-and-canonical-state.md).

When enabled, `std-utils` papercut reports persist unredacted model-supplied
operational text plus harness-routed agent/session identity and an operation
timestamp as plaintext per-instance `ExtensionDataScope::User` data. It is
shared across sessions that use one Tau state root and configured instance.
Tau-state operators can read these records, there is no automatic redaction,
and agents must not put secrets in reports. The harness serializes User-scope
appends across harness processes sharing that state root and instance. This is
explicitly best-effort diagnostic data: memory-only denial, quota/RPC failure,
and the accepted rare session-rollover mismatch can lose or misattribute a
record; ephemeral sessions intentionally use the same durable file. These
limitations do not expand the configured-local extension boundary or imply
hostile-process hardening.

`tau dev papercut list [--markdown]` and `clear` form a narrow local operator
exception for the normal `std-utils` instance's canonical `papercuts.jsonl`
file. They never enumerate arbitrary extension data. List opens the final
records file without following a symlink, requires one bounded regular UTF-8
v1 JSONL file with validated harness identifier fields and a renderable
timestamp, and renders only the recorded report plus its recorded attribution.
Plain output escapes controls; Markdown uses literal report blocks. Malformed,
unsupported, oversized, symlinked, non-regular, or unrenderable records fail
closed without exposing raw data.

Clear takes the same exclusive extension-directory lock as harness User-scope
appends, validates the same file, and removes it only while holding that lock.
It reports the number in its locked snapshot. An append that completed before
the lock boundary is cleared; an appender that waits for or starts after it
creates a new file and remains visible. A rejected input is never cleared.
Review this boundary when changing the papercut record schema, extension-data
file limit, User-scope lock, normal `std-utils` instance naming, or CLI output
sanitization.

The same configured-peer boundary admits transient `tool.progress_reported`
observations. The report commits before routed-call authorization; only the
downstream consumer may validate the captured live source, suppress backgrounded
calls, and publish immutable harness-sourced `tool.progress`. Parked reports
retain their original configured identity, and stale generations cannot produce
canonical progress. See
[`SPEC-tool-progress-reports-and-canonical-facts`](specs/SPEC-tool-progress-reports-and-canonical-facts.md).
Terminal Tool/Core reports use the same captured configured-generation
boundary. Mutable `tool.result_reported`, `tool.error_reported`, and
`tool.cancelled_reported` observations commit before exact routed-call
authorization and terminal state changes. Valid reports produce only immutable
harness-sourced terminal/provider/background facts; stale generations,
non-owners, completed calls, and direct canonical spoofs cannot close a call.
Reports and raw canonical result/error facts stay out of semantic journals,
while existing provider/cancellation/background transcript persistence is
unchanged. Ephemeral-agent classification suppresses raw reports and every
projection from durable debug JSONL. See
[`SPEC-terminal-tool-reports-and-canonical-outcomes`](specs/SPEC-terminal-tool-reports-and-canonical-outcomes.md).
Configured Tool/Core shell providers use the same captured-generation boundary
for `shell.command_progress_reported` and `shell.command_finished_reported`.
Reports commit before exact private-route and echoed-identity validation; only
the harness publishes canonical shell progress/completion. A stale, replaced,
non-owning, or identity-altered report cannot consume a route or inject
transcript output, and injection follows immutable canonical completion commit.
The publication envelope captures original-route ephemerality before
interception, replacement routes are reclassified from harness-owned pending
state or process-lifetime harness-generated ephemeral-route tombstones, and
unknown peer-chosen routes retain ordinary debug audit treatment. The tombstone
set grows only with accepted ephemeral user-shell routes until process exit.
The original frame-admission session must still match before a staged report
can derive canonical state.
See
[`SPEC-shell-command-reports-and-canonical-facts`](specs/SPEC-shell-command-reports-and-canonical-facts.md).
Configured Provider/Tool/Core peers may also publish `tool.request` routing
intents. The request commits before live generation, call-id correlation, and
registry routing checks; only harness-sourced derived facts assert acceptance
or terminal closure. Durable requests retain stable configured publisher
identity as a typed configured-extension provenance distinct from run-local
`ConnectionId`. Replay never interprets that provenance as a live source, never
routes it, and never executes work. See
[`SPEC-tool-requests-and-routing`](specs/SPEC-tool-requests-and-routing.md).
Configured local extensions are trusted to supply request agent correlation.
Internal-route correlation remains runtime/accounting state and never grants
agent-transcript ownership; its terminal projections are ownerless.
Separate pending correlation accepts progress and terminal reports only from the
exact routed owner; every accepted terminal or routed-owner disconnect clears
live request state and retains the completed-call tombstone. Re-check
request-to-terminal and internal-route closure tests whenever pending-call
ownership or report consumers change.
Every authenticated configured extension kind may publish transient prompt-fragment
declarations without a separate capability. The declaration commits before the
harness updates prompt assembly state; interception drop and stale connection
generations cannot mutate that projection. Pre-Ready declarations reserve bounded
activation capacity until commit, drop, or disconnect, and no declaration enters semantic
history or replay. See
[SPEC-prompt-fragment-declarations-and-projection](specs/SPEC-prompt-fragment-declarations-and-projection.md).
Every authenticated configured extension kind may also publish transient
session-provider registration, complete session/agent discovery source snapshots,
per-agent keyed context, and readiness without a capability. Post-commit consumers
revalidate the exact connection generation plus applicable session, agent, and
process-unique initialization id before mutating state. Only registered live
non-socket Tool subscribers participate in captured waits; per-agent readiness
cannot release session readiness or another agent. A connected effective waiter
has no deadline and can hold initialization until acknowledgement or disconnect.
Pre-Ready declarations reserve bounded activation count/bytes, and snapshot
validation additionally bounds item count, decoded bytes, and individual AGENTS.md
content. Invalid items are diagnosed and omitted before one atomic source swap.
Raw declarations never enter semantic journals or replay. Re-check reservation
cleanup, wait admission, atomic source replacement, and disconnect/respawn
generation checks whenever this flow changes. See
[SPEC-session-discovery-declarations-and-readiness](specs/SPEC-session-discovery-declarations-and-readiness.md).
The harness treats
`agent.initialization_context_set`, `harness.agent_context_initialized`, and
`harness.session_skills_available` as protected harness-authored events:
configured extensions and attached UIs cannot publish them, and interceptors
cannot drop or mutate them. Only `agent.initialization_context_set` defaults to
durable publication and folds as replaceable agent side state rather than
transcript history; every finalization appends the exact fresh initialization-ID
replacement even when effective content is unchanged. Both `harness.*` events
are transient current projections synthesized for late subscribers.
Finalized agents consume one frozen snapshot for prompt skill listing, model/user
skill lookup, and AGENTS.md bootstrap materialization.

Skill metadata snapshots file paths and mtimes, not file bodies. A local file may
change between scan and later skill loading, so discovery is not a content-integrity
or filesystem sandbox boundary. AGENTS.md content is carried in the snapshot and
frozen at initialization. Treat all discovered files as trusted local prompt input.
See
[SPEC-per-agent-context-declarations-and-readiness](specs/SPEC-per-agent-context-declarations-and-readiness.md).
Every configured extension kind may publish transient internal-prompt requests.
The harness commits them before loaded-agent validation, revalidates the exact
connection generation, and excludes raw requests from semantic replay. Invalid
targets remain observable but cannot create prompt facts; stale generations
cannot submit work. See
[SPEC-internal-prompt-submit-requests](specs/SPEC-internal-prompt-submit-requests.md).
Every configured extension kind may also publish transient start-agent requests.
The harness commits the raw request before revalidating the exact live generation
and admission session, then applies role, parent/tool-owner, duplicate rebinding,
and child-creation logic. Unconfigured/socket peers are denied; stale generations
or sessions are observation-only, and raw requests never enter semantic replay. See
[SPEC-start-agent-requests](specs/SPEC-start-agent-requests.md).
Every live configured extension kind and attached local UI may publish terminal
bell and OSC side-effect events through ordinary interception and commit.
Unconfigured, disconnected, and dedicated external-message peers cannot. The
events never enter semantic stores, and terminal UIs independently reject replay
delivery before writing terminal bytes. OSC name validation and bounded,
base64-encoded values remain defense in depth. See
[SPEC-terminal-output-side-effect-events](specs/SPEC-terminal-output-side-effect-events.md).
Every live configured extension kind and attached local UI may also publish
custom events under extension-owned categories. Unconfigured, disconnected,
non-UI socket, and dedicated external-message peers cannot. Structural name
validation prevents custom payloads from spoofing typed first-party event names;
opaque payloads remain live/debug-only and never enter semantic stores. Existing
trusted-local frame, activation, and diagnostics-cardinality bounds apply. See
[SPEC-custom-extension-events](specs/SPEC-custom-extension-events.md).
Only harness-assigned attached socket UIs may publish prompt-draft and focus
liveness observations. Dedicated external-message, non-UI, disconnected, and
extension-path peers cannot. Drafts contain the full current prompt buffer and
remain visible to privileged interceptors, subscribed live peers, and debug
logging, but neither liveness event enters semantic stores or replay for either
`persist` value. The shared decoded-message bound applies; no smaller trusted-UI
payload bound is promised. See
[SPEC-ui-prompt-draft-and-focus-events](specs/SPEC-ui-prompt-draft-and-focus-events.md).
Configured Provider peers likewise submit transient
`provider.quota_*_reported` observations before any account-state acceptance.
Only the post-commit consumer may validate the captured live generation,
provider/model-route ownership, bounded records, and epoch/sequence transition,
then publish protected harness-sourced `harness.provider_quota_changed`.
Unowned or stale reports may remain observable as committed observations but
cannot mutate current quota state. Neither reports nor canonical snapshots enter
semantic journals or cold replay; they contain no credentials or account IDs.
See [SPEC-provider-quota-pacing](specs/SPEC-provider-quota-pacing.md).
Configured Provider execution uses five transient `_reported` observations through the
same trusted local boundary. Reports commit before exact live-generation and
prompt/retry correlation; only harness-sourced successors assert canonical execution
facts or directed retry outcomes. Reports are excluded from semantic journals for
either supplied `persist` value. Raw terminal report delivery/debug projections clear
provider-image bytes. This boundary validates routing and lifecycle ownership; it does
not treat configured provider payloads as hostile extension input or add spoofing
hardening. See
[SPEC-provider-execution-reports-and-canonical-facts](specs/SPEC-provider-execution-reports-and-canonical-facts.md).
Committed terminal result reports may carry typed provider images for downstream
validation, but every debug JSONL projection clears image bytes under
[`SPEC-typed-image-tool-results`](specs/SPEC-typed-image-tool-results.md); only
validated provider transcript storage and directed provider prompts retain
canonical bytes.
Generic configured-extension spawn diagnostics treat the configured instance
name, resolved executable, and explicitly configured cwd as non-secret metadata;
do not place credentials or tokens in those fields. Diagnostics bound and escape
those fields, include cwd only when configured, and preserve the underlying
operating-system error/source chain. They never retain or render command
arguments, full extension configuration, environment values, or resolved secret
values. Re-check this contract whenever extension spawn configuration or
startup/respawn logging changes.
Per-agent shell-instance workdirs are committed coordination state, not an
access-control boundary. Paths are interpreted only by the configured extension
instance that owns them. Malformed or unavailable remembered paths fail closed
until explicitly repaired, and user-shell routing failures remain local command
failures rather than changing extension authority. Directory locks remain
advisory coordination.
Robust framing and cleanup improvements are welcome when scoped, but unrelated
features must not be expanded into slowloris, connection-flood, or sandbox
hardening without an approved threat-model design.

Runtime discovery is non-destructive under the cooperative local boundary.
Daemon runtime pairs use `<pid>-<16-lowercase-hex-instance>` stems so separate
PID namespaces sharing one runtime directory cannot collide on an active path.
A CLI-owned launch mints one instance value, passes its validated form to the
child, and derives the same path itself; direct launches mint locally.
Checking a metadata PID, socket reachability, and pathname identity cannot be
atomic with PID reuse and a daemon replacing that pathname, so scanners must
not unlink apparently stale lifecycle pairs. Owned CLI shutdown closes the
initial transport first so the daemon normally removes its own pair, with
bounded forced termination retained as a last-resort availability safeguard.
Targeted session lookup bounds raw traversal, matching candidates, metadata
bytes, and total time and fails closed when uniqueness remains unproven,
including unreadable current PID-prefixed or legacy numeric metadata owned by
a live or liveness-unknown PID.
Local running-session listing isolates bounded raw traversal from its caller and
uses runtime paths only as socket candidates. Each responsive daemon returns its
in-memory current session id and immutable canonical startup project root
through a correlation-matched, per-probe-deadline local socket RPC; adjacent
metadata and persisted session directories supply neither records nor fields.
The CLI escapes record separators and terminal controls before writing
line-oriented output, and uses JSON string escaping for structured output.

Inter-harness/session communication is likewise cooperative same-UID IPC, with
correlation and bounded model-spend admission rather than hostile-sender ACLs.
Genuinely untrusted ingress is external network/service content received through
Slack, XMPP, Telegram, providers, web fetches, and similar adapters. Authenticate
and bound that adapter boundary where applicable and keep payloads untrusted model
content; proxying them through an extension does not make the local extension
transport itself adversarial. The boundary summary is recorded in
[`ARCH-external-message-boundary`](specs/ARCH-external-message-boundary.md).

Successful `tau-ext-websearch` results remain ordinary invocation-correlated
tool-result strings. The extension places Exa search and Parallel search/fetch
text inside one exact-close-framed `<tau_web_content>` boundary with closed adapter,
operation, and external-trust labels, and enforces its result bound after
framing and closure. Adapter identity authenticates neither page authorship nor
truth; provider titles, URLs, ranks, sources, and prose remain untrusted body
claims capable of prompt injection. The envelope prevents exact closing-sentinel
breakout but is not a sandbox or instruction-authority change. See
[`SPEC-tau-ext-websearch-provider-boundary`](crates/tau-ext-websearch/specs/SPEC-tau-ext-websearch-provider-boundary.md)
and
[`SPEC-tau-ext-websearch-runtime-safeguards`](crates/tau-ext-websearch/specs/SPEC-tau-ext-websearch-runtime-safeguards.md).

The Slack bridge requires exact configured conversation/kind/thread policy and
verified live-human admission. Receive permission creates only Tau-issued
source-bound reply authority; proactive permission is a separate alias-only
grant. Dynamic DMs remain bounded, allowlist/exact-user-bound, and reply-only.
Slack submits transient message reports through ordinary interception; the
harness retains each raw publisher claim losslessly for observation and audit.
The harness publishes an immutable canonical fact only when the top-level claim
is grammar-valid and exactly matches the authenticated configured extension
name, then stamps canonical provenance from that captured identity. A malformed
or mismatched claim remains only a transient report. Nested message references
remain opaque and follow projection validation rather than report admission.
Actionable reply and reaction authority stays in extension-local runtime state.
The Slack extension drops
recently repeated native occurrence ids with a bounded process-local cache before
report submission. Generic event infrastructure does no native deduplication or
ownership resolution: each canonical fact is a new immutable occurrence.
Cache eviction, restart, or races may therefore duplicate delivery.
Slack records an occurrence before identity lookup, local effects, capacity
admission, and local report write; a later transient failure consumes that
occurrence until eviction or restart rather than retrying it.
Use one Slack extension instance for one receiving agent as specified by
[`ARCH-tau-ext-slack`](crates/tau-ext-slack/specs/ARCH-tau-ext-slack.md).
Re-check record-before-submission ordering, Slack-local cache bounds, and
disconnect/session route cleanup whenever Slack report submission changes.
Report flush acknowledges only submission to the local protocol writer, not
canonical commit. Interception, append failure, or a crash can therefore leave a
remote/local effect without a canonical fact.
The separately authorized, default-off Slack reaction tool accepts only locally
retained exact-message refs from submitted reports/results, requires current route
and role authority, and permits removal only of same-agent runtime-owned
reactions. It adds `reactions:write` without reaction listing; reactions are
externally visible and can trigger notifications or workflows.

The separately authorized Slack discovery tool reveals all static model-facing
aliases and configured policy, including receive-only routes, but excludes native
routes, dynamic links, identities, runtime state, and Slack-fetched metadata.
`security_mode: lax` materially widens prompt-injection exposure on static
routes and must not be treated as control authority. Slack, workspace
administrators, Slack Connect participants, and conversation members may read
transported text; this is not an end-to-end encrypted channel.
Slack additionally binds `auth.test` bot/workspace identity to each supported
Events API wrapper and fails closed on missing, ambiguous, or mismatched
installation evidence. Native U/W ids remain authoritative; fetched display
names and operator aliases are presentation only. Agent sends reject raw Slack
mention/control markup; the optional source-mention field can name only the
verified human already bound to a live reply selector and is frozen with the
bounded retry body.
Slack-specific review triggers and failure/replay invariants are recorded in
[`crates/tau-ext-slack/SECURITY.md`](crates/tau-ext-slack/SECURITY.md).

The disabled-by-default Zulip bridge uses bot HTTP Basic authentication and
native event-queue long polling. Exact numeric sender allowlists and configured
DM or stream/topic policies gate ingress; separately configured proactive-DM
aliases fix outbound recipient authority. Queue identifiers and native
participant, stream, topic, and message routes remain extension-local authority;
model tools accept only current configured aliases or bounded Tau-issued source
references. With the default `offline_message_catch_up: false`, queue expiry
reconnects at a fresh live tip and warns about a possible gap rather than
fetching missed backlog. Opting into `offline_message_catch_up: true` uses the
extension-local `CheckpointRuntime` to recover bounded newly created messages
after its durable position. Successful sends report before
their tool result, while ambiguous provider outcomes are not automatically
retried. Zulip-specific review triggers are recorded in
[`crates/tau-ext-zulip/SECURITY.md`](crates/tau-ext-zulip/SECURITY.md).

## Standalone compaction recovery reliability

Standalone compaction and its continuation are harness-owned durable work. Every
new provider cut must be a closed transcript prefix; a tool-calling assistant
response and its complete terminal results node are indivisible. A failed
transaction with a resume watermark remains fail-closed until an explicit
successor preserves same-branch coverage of that watermark. A successor may
retreat its cut to retain more exact suffix, but it must not replace the owed
watermark with an ancestor or sibling selected by later head navigation.
Ordinary input and `:cancel` do not abandon this ownership; if the selected head
no longer descends from the owed watermark, explicit recovery must remain
blocked. Core validation and warm/cold replay regressions enforce these rules.
Revisit them when adding any explicit abandon/rewind operation or changing
compaction replay ownership.

Exact committed publication envelopes create or transfer activation ownership.
A retained completion envelope or standalone `AwaitingCheckpoint` tuple represents
durable work and remains bound to its owning branch. Queue/in-flight attempt
markers are ephemeral: every prevalidation or persistence rejection clears them,
and agent unload or session rollover discards all warm-process retry state.
Transaction-owned publications carry their enqueue-time session generation and
must still match an exact live runtime owner before commit. Destructive lifecycle
cancellation suspends that interceptor's registration until its one outstanding
uncorrelated stale reply is consumed, so the reply cannot bind to later session
work without changing the extension connection lifecycle. Registration
replacement remains suspended, no timeout applies, exactly one reply is consumed,
and disconnect clears suspension. The interface contract is specified by
[`SPEC-tau-harness-event-processing`](crates/tau-harness/specs/SPEC-tau-harness-event-processing.md).
Unrelated accepted publications retain FIFO order and complete or fail through
their normal path.
Rollover advances the session admission generation before quiescence. Raw
session-bound events whose contracts require observation may still commit, but a
central post-commit peer guard suppresses their semantic effects and releases
activation reservations. Process-global tool/prompt-fragment/model declarations
and provider-quota current-state reports are explicit exceptions: they survive
rollover only while exact captured connection/instance identity remains current.

The model-callable self `compact` capability is enabled by default and can act
only on the calling agent. Effective role policy may revoke it by exact tool
name, compaction group, or matching tag. The cross-agent `agent_compact`
capability remains independently disabled by default; explicitly granting it
authorizes compaction of another loaded same-session agent but does not alter
self-compaction policy.

Ordinary-inference cancellation may release only the exact matching warm-process
`DispatchUncertain` owner so later work on that agent can proceed. Its transient
terminal and late-response rejection do not establish crash-exact cancellation
persistence; standalone-compaction ownership remains covered by the durable
rules above.

## Named context-size alert reliability

Named context-size alerts are operator-configured, model-visible advisory
prompts. Provider token usage may trigger an alert only for an accepted,
successful, non-compacting ordinary response under the policy captured for that
prompt. Canceled, stale, duplicate, failed, and compaction responses create no
alert work. Alert messages grant no tool authority: `compact` remains separately
controlled by effective tool policy.

Crossing suppression and still-queued alert delivery are daemon-local
best-effort state, not durable recovery obligations. A crash can lose an
uncommitted queued alert, and a later successful response after cold replay can
evaluate restored high usage again. Once delivery commits, the submitted or
steered prompt fact is durable and cold replay preserves its journal position.
Its harness-owned `internal_kind=context_size_alert` tag and exact configured
text are protected against interceptor addition, removal, or rewrite; missing
tags remain hidden, and neither `ctx_id` nor text can infer the tag. Tau creates
no synthetic replay event. Warm-process regressions cover threshold crossings,
failure exclusion, tool-round deferral, accounting resets (including stale
queued-alert removal), and prompt-owned role policy; cold-resume regressions
cover both submitted and steered tagged delivery facts.

## Release build resource reliability

The universal release binary's accepted build-time, memory, size, and runtime
tradeoffs, measurements, temporary adoption limits, and re-evaluation triggers
are documented in [`docs/release-builds.md`](docs/release-builds.md).

## Reporting guidance

When reporting a vulnerability, include:

- affected Tau version or commit;
- operating system and relevant configuration;
- minimal reproduction steps;
- whether an extension, provider, UI client, or daemon boundary is involved;
- any logs that do not contain secrets.

Avoid sharing API keys, OAuth tokens, email contents, or other private data in
reports.

## Agent navigation modes

Navigation modes are same-user UI control state with presentation-only effects.
The harness accepts absolute mutations only through authenticated attached-local
UI intake; extensions and external peers cannot mutate them. This authority
covers explicit navigation requests and the implicit `active` write after a
visible human prompt is durably admitted for an existing target. Payload
`originator`, submission provenance, durable prompt replay, and later queue/steer
processing are not authentication and cannot independently cause the write.
The authenticated cooperative bare peer-entrypoint auto-start has one
harness-internal exception: after durable identity and current-session membership
setup, the harness writes `active` only for its newly created recipient and
publishes complete stats. The peer never chooses a mode, and exact/existing
recipients and all other start paths cannot acquire this write. The runtime-only
classification is forgotten on unload, session switch, or process exit; cold
restore recomputes the extension-origin `active_auto` default. Receive-commit ACK
authority remains independent of this UI-only state.
Modes do not authorize loading, routing, prompt delivery, watches, execution, or
model access and are intentionally not durable.

The directed agent-roster RPC is available only to same-user local connections
classified as UI clients. It exposes stable ids, lifecycle/persistence,
navigation/runtime status, creation role/parent/time, and a verified display
name, including unloaded history when requested. Live rows also include the
agent's current self-reported work-status phase and model-authored title. Titles
are untrusted presentation metadata; the picker visibly escapes them before
passing roster rows to the trusted local `fzf`. These are content-minimized
coordination labels, not secrets or an authorization boundary. Results go only
to the requester and never enter event publication, interception, subscription
replay, or extension delivery.
The harness seeds roster caches atomically from validated committed membership
before runtime restoration and updates them only after later membership commits;
any restore/commit failure invalidates the projection. Entry count is checked
before ids are cloned. Creation records, checkpoint reads,
in-memory ephemeral projections, intermediate encoding, and the final protocol
message are bounded before allocation or transmission. Malformed creation facts
remain categorical without repair, locking, or writes; a cold display name is
used only when its checkpoint identity and boundary still bind it to the exact
journal. Snapshot failures return no partial rows.

The optional picker resolves `fzf` through the same user's `PATH` and therefore
treats it as trusted local code. Tau invokes fixed arguments directly, bounds its
stdin/stdout and runtime, restores foreground ownership and raw terminal state,
and revalidates the selected agent. Cancellation, subprocess/RPC errors, and stale
selection are no-mutation outcomes.
Picker membership follows live lifecycle/navigation authority independently of
missing, invalid, or unreadable creation-fact enrichment. Selection never
changes navigation mode, runtime state, or agent loading.

Session activity reports trust only harness-authored canonical journal facts.
Provider-supplied accounting fields are discarded before canonical publication;
captured response-local usage, effective rates, and increments are immutable
authority. Outer-turn lifecycle and prompt joins are immutable must-pass facts.
Stats traversal is read-only and performs no repair, migration, configuration
lookup, or inferred backfill. Missing accounting authority makes `complete` false;
malformed or corrupt journals fail closed. An unmatched crash-cut start remains
reported as unterminated but does not block a later boot's fresh turn.

Changes to this boundary must re-check strict replay lifecycle joins, cross-session
prompt/response/tool isolation (including reused call IDs), interception
drop/replacement and peer-forgery rejection, response-local rather than cumulative
usage accounting, and the no-write behavior of offline inspection.

## Offline agent trace export

`tau agent trace` exports unredacted durable journals. Output can contain full
prompts, reasoning, images, tool arguments and results, messages, model
parameters, usage, and cost data. Treat native, OTLP, compact JSONL, and compact
TOON output as sensitive as the original state directory; redirect or transmit
it only to trusted destinations. Compact lite mode exposes tool names, arguments,
commands, statuses, output sizes, and up to 4 KiB of unredacted normalized output
per terminal call, including bounded rendered error details. Full mode exposes
complete normalized output and rendered error details.

The exporter opens only existing state and never repairs or writes it. Only
writer-lock contention selects checkpoint mode. Inactive journals acquire their
exclusive locks before opening the journal and selecting EOF; lock-held journals
retain one exact opened journal identity and select a finite prefix through the
existing bound checkpoint. Bounded positional reads and strict replay validate
every selected prefix, descendant discovery is rechecked after capture, and all
failures occur before stdout. Capture never waits for lock release or includes
records committed after its selected cuts. A snapshot retains one journal file
descriptor per included agent plus lock descriptors for inactive agents until
private staging finishes. A very large selected workflow can exhaust process
file descriptors and fail before output.

Descendant discovery accepts only a valid matching sequence-zero
`AgentStarted.creator` record as an authenticated edge. Missing, unreadable,
unsupported, or invalid candidate first records establish no edge and remain
outside the rooted workflow. Every selected prefix still requires strict
semantic validation, so unsupported or corrupt content inside that prefix fails
before output. Keep `agent_trace_descendants_ignore_unrelated_legacy_creation_record`
and `agent_trace_descendants_reject_reachable_corrupt_journal` as regression
safeguards when this boundary changes.

Private staging is an anonymous process-owned file with no pathname to survive
termination and is never durable trace state. Validation and projection stream
journal records. OTLP keeps every correlated occurrence in anonymous staging,
including auxiliary occurrences whose offsets are not retained; heap
correlation state retains compact offsets and identifiers, one per unique typed
operation key.

Heap use is proportional to unique operation-ID count and bytes in the largest
included journal; IDs have no separate cap beyond the record framing limit.
A pathological journal can therefore exhaust exporter process memory. The
exporter never truncates accepted records.

Compact projections materialize selected source events and projected records in
memory. Lite bounds each semantic text and terminal output projection to 4 KiB,
but declaration arguments and retained selected events remain bounded only by
journal framing. Full mode retains complete semantic text and rendered output.
Heap can therefore grow with selected journal payload bytes and projected record
bytes. TOON escapes multiline strings and must never print payload C0/C1 controls
raw.

The performance projection emits no prompt, tool, response, or error bodies, but
its agent/prompt/model IDs, descendant membership, timing, token/cache counts,
and estimated cost remain sensitive metadata. It retains only compact
response-local accounting, provider-qualified model identity, and timestamps
per prompt, not provider payloads, model parameters, or cumulative usage
snapshots. Heap remains proportional to prompt correlation count and
agent/prompt/model identifier bytes. Changes must recheck zero and decreasing wall timestamps, duplicate
terminals, checked aggregate overflow, zero/fractional cache-ratio boundaries,
and structural output-field privacy.

One pathological frame-valid selected journal can exhaust memory or temporary
storage. Projection failure remains before stdout, and the final anonymous file
remains delete-on-close.

Changes to compact projection must re-check zero/many-call framing, arbitrary
strings and controls, strict TOON semantic round trips, exact tagged-CBOR and
float-bit reconstruction, multiline full output, and parity with independently
parsed JSONL items across all semantic families.

Revisit this boundary before adding user-selected output files, redaction modes,
provider HTTP-body or streaming-delta capture, new timing authority, or any
persisted trace state.

## Tau Swarm extension

`std-swarm` is a configured local extension that connects to one
cryptographically pinned Iroh endpoint. The configured credential is sensitive;
Tau supplies it only through the declared Configure secret and the extension
must not log it. Relay and direct addresses are reachability hints and do not
weaken endpoint identity verification.

Remote prompts and blocker answers reach agents only through Tau's canonical
internal-prompt path. The extension retains command deduplication, blocker
history, and unacknowledged updates in process memory under configured bounds.
Tau Swarm 0.2.0 binds commands and active lifecycle state to a collision-resistant
extension-process incarnation. Ordinary reconnects and session switches retain
the process command table; a replacement process declares a fresh incarnation,
so the server fences ambiguous old commands and supersedes old active lifecycle
state. A peer that sends many unique, otherwise valid commands can fill the
no-eviction command table and deny later remote commands until process restart. Large
configured bounds can exhaust extension memory; they are operator trust and
capacity choices rather than untrusted local-IPC hardening boundaries.

### Compact semantic trace disclosure

Compact lite traces expose up to 4 KiB each of unredacted assistant prose, displayable reasoning, explicit sent/received message text, and tool output, in addition to complete tool arguments. Full mode exposes complete text and output. Reasoning and messages can contain secrets, private communications, user data, or model-derived sensitive content; paired directional records can duplicate the same sensitive body across included journals. Absolute timestamps, agent/session/message/prompt IDs, membership, and activity patterns are sensitive metadata. Cross-agent wall-clock order is not causality.

Compact identity, sequence, and timestamp fields remain facts of the captured
journals when those journals move between sessions; export never rebinds them to
the containing session. Original-host wall-clock samples can regress or differ
across hosts and do not establish delivery order, latency, or happens-before.
Tau keeps extension credentials in harness-mediated configured-instance Secret
scope. Supervised extensions run in mandatory fail-closed Linux user and mount
namespaces that hide the whole Tau secret root. Only configured Provider
instances receive credential-free provider settings: persistent instances get a
read-only mount and `Configure.settings_files`, while memory-only previews get
only an immutable `Configure.settings_files` snapshot. Tool instances receive
neither. This is defense in depth for trusted configured same-UID executables, not containment
from malicious same-UID code or misuse of credentials returned to an authorized
extension. Secret payloads remain absent from logs, events, journals, generic
debug output, and errors. See
[`SPEC-extension-secret-storage`](specs/SPEC-extension-secret-storage.md).

Named provider API-key bindings use one closed parser shared by setup, harness,
and provider runtime. The harness consumes the configured declaration without
forwarding its value in `Configure.secrets`, refreshes only canonical API-key
records, and replaces unavailable bindings with empty typed records plus
value-redacted warnings. A per-instance provider-settings lock precedes the
Secret-scope lock in setup, removal, and startup, binding source selection and
credential publication to the exact retained Configure snapshot. Typed
Tau-component identity, rather than executable argv resemblance, grants this
authority. Source read/decoding failures preserve older records and fail or skip
the provider according to its required policy. Memory-only startup forwards no
built-in provider declaration values.
