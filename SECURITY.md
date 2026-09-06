# Security policy

Provider-hosted web search runs inside the selected inference provider and does
not cross Tau's registered-tool dispatch boundary. Search queries, actions,
returned content, URLs, titles, and citation metadata are untrusted external
data; none grant Tau identity, instruction, authorization, or routing authority.
Cached hosted access still contacts the provider and is not an offline or
privacy boundary.
`allowed_domains` is not a network sandbox. Hosted filtering and model-chosen
browsing are delegated to the inference provider. External fetch gates only the
requested target before extractor contact; redirects and subresources may leave
the allowlist. A nonempty allowlist makes ordinary candidates without declared
enforcement unavailable. The default external search pool declares no such
enforcement, so Tau omits it rather than calling and post-filtering; configured
Tavily or Firecrawl adapters can carry their provider-side filters.

Durable semantic filesystem mutation belongs to one Harness-lifecycle
`SemanticPersistenceOwner`. Stores pass generation capabilities and staged
replacements to its sole worker; configured local extensions receive neither
journal handles nor persistence capabilities. Read-only inspection constructors
cannot acquire live mutation authority. This does not change the documented
cooperative local-extension threat model.

The `tau agent unload` control RPC is restricted to an authenticated attached
same-user socket UI. Its request and result are directed, transient,
non-broadcast, non-intercepted, and non-journaled control messages; configured
extensions receive only the ordinary committed `session.agent_unloaded` fact.
Admission rejects any target with accepted work, and the command preserves agent
transcripts and session history rather than editing or deleting durable state.

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
creation, direct-IP public mode, shared database, or memory-only fallback. The
separately maintained `tau-ext-rostra` project owns its detailed architecture
and security documentation.

The optional `std-swarm` configured extension is a trusted same-user local
executable. Its remote Iroh peer is cooperative but authenticated and
identity-pinned before the worker credential is sent; externally supplied
prompts, answers, identifiers, and collections remain size-validated inputs.


Tau is early-stage software, but security issues are important. Please report suspected vulnerabilities through GitHub private vulnerability reporting for `dpc/tau` (<https://github.com/dpc/tau/security/advisories/new>) when available. If that path is unavailable, contact the maintainer privately first and avoid filing a public issue with exploit details.

Foreground serve bootstrap sources are local configuration authority and may
contain secrets. Tau reads the selected file only after readiness and never logs
its contents, but the path can appear in process metadata or operational errors.
Use owner-private service-manager credentials rather than Nix-store paths.
Bootstrap submission reuses the authenticated same-user local UI boundary; a
harness-private request registration alone may create the reserved durable
bootstrap marker, and public metadata producers cannot set or inherit it.

For technical trust boundaries, start with [ARCH-external-message-boundary](specs/ARCH-external-message-boundary.md) and the applicable project and component records under `specs/` and `crates/*/specs/`.

### Harness configuration authority selection

Before launching any configured process, Tau accepts one normalized effective
`HarnessSettings` snapshot. Missing optional user configuration leaves the
built-in layer valid, while an existing unreadable or invalid layer and malformed
private override transport fail startup with context instead of selecting
built-in fallback authority. Extension launch, provider and secret inputs, roles,
tool policy, retention, prompts, and runtime baselines derive from that snapshot.
An already-running harness retains it and does not hot-reload or partially reread
settings. This authority-selection invariant does not change the trusted
same-user extension threat model.

See
[`ARCH-tau-config`](crates/tau-config/specs/ARCH-tau-config.md) and
[`ARCH-tau-harness`](crates/tau-harness/specs/ARCH-tau-harness.md).
Revisit this invariant when changing loader fallback or error semantics, snapshot
plumbing, configured-process launch ordering, or runtime settings lookups.

### Accepted configured-component availability risks

Registered session-context providers may delay session initialization, but only
within a non-renewable thirty-second absolute cap. Exact readiness or disconnect
from each outstanding provider owns early completion; provider silence and
generic, stale, wrong-session, or non-outstanding traffic cannot complete or
extend the wait. Final waiter removal and harness-owned synchronous finalization
take precedence over timeout classification, while absolute expiry returns
`SessionInitTimeout` rather than process `StartupTimeout`. Deterministic deadline
and classification boundaries are covered in
`crates/tau-harness/src/session_init_deadline/tests.rs` and
`crates/tau-harness/src/error/tests.rs`; lifecycle tests cover admission and
finalization wiring. Revisit these safeguards when changing waiter identity,
admission filtering, event ordering, finalization, or timeout classification.
The governing contract is
[SPEC-session-discovery-declarations-and-readiness](specs/SPEC-session-discovery-declarations-and-readiness.md).

A configured extension's process-local client writer retains detached outbound
frames in one FIFO capped at 64 frames and 8 MiB of aggregate encoded data, with
an independent 8-MiB limit per frame. This is an availability safeguard inside
the trusted same-user extension boundary, not hostile local-IPC containment.
Accepted pre-Ready frames drain in order after `Ready`, `ConfigError`, or graceful
shutdown; bounded drain batches prevent continuous detached production from
starving synchronous writer commands. Exact boundary, ordering, refill/recovery,
and blocked-writer tests protect these properties. Revisit them when changing
writer notification, FIFO budget release, startup activation, ConfigError
ordering, or writer shutdown.

A connected consumer generation that stops advancing may pin shared live-event
payload retention indefinitely. Tau emits only a rate-limited warning; it does
not backpressure or reject publication because of egress lag, spool the suffix
to disk, or expire/disconnect the component solely for lag. Revisit this
accepted trusted-component risk when changing live routing, cursor identity,
target freezing, pruning, or lag diagnostics and lifecycle.

A connected trusted interceptor may leave an intercept request unanswered and
thereby stall that publication plus every globally serialized publication
behind it indefinitely. Deferred publications retain their full events and may
consume memory without bound. Tau deliberately applies no timeout, admission
budget, rejection, quarantine/disconnect, or backpressure mitigation. Revisit
this accepted authority and availability risk when changing interception,
pending-intercept resolution, deferred-publication ownership, or component
lifecycle.

Disabled-by-default decoded-delivery memory diagnostics report only process
class, fixed ownership-cut labels, counts, encoded bytes, recursive logical and
requested-capacity estimates, expansion, shared-allocation fanout, and
high-water aggregates. They emit no payload, protocol/runtime identity, cursor,
path, model, account, or error value and create no event, journal, debug-JSON,
wire, or cross-process authority. Requested capacity is explicitly an
enabled-only diagnostic projection estimate; allocator usable size, RSS, and
kernel/socket buffer ownership remain unobserved. Exact aggregate sizes, ratios,
fanout, and timestamps may still reveal workload metadata or permit heuristic
cross-process correlation; treat enabled operational trace files as private.

Disabled-by-default provider backend-stage diagnostics use the dedicated
`provider.backend-stages` TRACE target. They report only closed backend,
transport, and outcome classes plus scalar durations, byte counts, occurrence
counts, presence flags, and accounted/unattributed totals. They never retain or
emit prompts, response content, model or prompt identifiers, endpoints, paths,
accounts, credentials, status bodies, or raw errors, and create no event,
journal, capture artifact, wire field, or protocol authority. Exact timing and
size values remain private workload metadata. With the target disabled,
backends retain only inert `None` checks and perform no diagnostic clock reads,
allocation, byte sizing, hashing, I/O, or trace-state construction.

Disabled-by-default provider/client output-cost diagnostics use the dedicated
`provider-builtin.output-cost` and `tau_client::output_cost` TRACE targets.
They report only fixed phase/lane/outcome classes, process-local numeric
correlation, item and queue counts, encoded byte counts, and scalar durations
for sampler materialization, worker admission/queue/drain, client admission,
writer wait, encoding, and flush. They never retain or emit prompt, response,
tool, error, model, endpoint, account, session, protocol-field, or credential
values and create no event, journal, capture, wire field, debug JSON, or
protocol message. Like other extension tracing, enabled provider observations
can flow through extension stderr into the owner-private per-session component
log and optional operational mirror; client observations use the process's
configured tracing sink. Exact sizes, timing, ordinal correlation, and nearby
log context remain private workload metadata and can permit heuristic
cross-correlation. With both targets disabled, the
paths retain only inert `Option::None` checks/moves and perform no
diagnostic-specific clock read, allocation, traversal, queue accounting, or
trace construction.

Overall harness shutdown closes configured in-process extension transport first,
then gives all such runners one shared finite grace to return on EOF. A runner
still alive after that grace is left to a detached join-reaper, not
force-cancelled; it can retain arbitrary resources and continue side effects
until host-process exit. Normal Tau process exit reclaims those resources, but
embedded or reusable hosts can accumulate detached runners across
shutdown/restart cycles. Revisit this accepted configured-component availability
risk when changing extension lifecycle, the shared cleanup grace, or
embedding/reusable-host behavior.

Supervised extension state isolation is defense in depth inside the configured
same-user executable boundary, not hostile-code containment. `hidden` and
`read_only` prevent ordinary discovery or mutation of unrelated Tau state while
preserving exact extension-owned state; they do
not defend against procfs, ptrace, pre-opened descriptors, unrelated host data,
or authorized secret RPC delivery.
`read_only` is the persistent-harness default: it improves debugging and
cooperative extension introspection, but every configured supervised extension
in a persistent harness can therefore read other Tau session and agent state
unless its policy is explicitly `hidden`. Memory-only harnesses force `hidden`,
create no host state, and mask an existing state root if one exists.
Supervised components also receive an empty bind-mounted harness runtime socket
directory by default. A per-component `tau_runtime_socket_access: legacy`
opt-out restores ambient socket discovery for a trusted component without
changing its state or secret access.
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
Historical explicitly enabled compact HTTP failure captures preserve bounded causal
provider evidence, including an allowlisted header set and a credential-redacted
64-KiB decoded body prefix. Reqwest content decoding precedes accounting; captures
hash exactly the decoded bytes delivered and distinguish complete decoded-body
from partial coverage. Treat these owner-only local artifacts as
sensitive: provider error bodies can reflect prompt, account, or service-internal
data even after configured credentials are removed. Configurable diagnostic
retention defaults to thirty days; disabling cleanup can retain them
indefinitely.
Default-on Codex, public Responses and Chat Completions inference and
standalone-compaction scalar cache diagnostics use the same
private opaque path and retention. Metadata has an independent startup-frozen
profile opt-out and is forced off with existing nonpersistable capture policy.
Existing exact captures remain default-on for durable activity; native Codex
successful raw compact responses remain unretained and advertise
`exact_response=false`.
Scalar records omit payloads, credentials, routes, cache keys, provider IDs and
error prose, but retain bounded model identities and workload correlation, so
they remain private rather than public-safe. The metadata budget reserves at
most 64 records / 16 MiB including in-flight serialized data, with a 256-KiB
per-record cap. Sequence holes and known-loss counters never prove complete
capture history. Neither capture failures nor metadata observations affect
provider execution, canonical accounting, cache eligibility, or canonical
compaction outcome. Public Responses and Chat Completions local-summary attempt
ends describe the finite backend result before extension-owned narrative
validation.
Codex warm backend calls use scalar-only operation captures, with no prompt
identity and no retained exact request/response. Their closed operation
attribution is permitted only for cache diagnostics and uses a distinct managed
filename. Backend completion is not authenticated refresh-terminal acceptance.
Disabled-by-default cache refreshes resend an exact previously successful
Provider-visible prefix. The harness keeps that content in process memory and
sends refresh/cancel requests point-to-point only to the captured configured
Provider generation. Sensitive requests are excluded from broadcast,
interception, journals, replay, debug JSONL, generic Debug output, watchers, and
UI projection. Content-free terminal reports and canonical facts may remain
observable. The Codex warm path receives the prefix through trusted directed IPC
and sends it upstream, but retains no exact warm request or response capture.
Its private scalar operation evidence does not retain prefix content.
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
The serve-only `--mirror-extension-stderr` option similarly treats arbitrary
supervised-child stderr as untrusted display bytes: it emits bounded escaped
records with validated extension, immutable generation, and PID attribution,
never raw child bytes. The authoritative private file is written and flushed
first. A bounded nonblocking queue may drop mirror records, and process-stderr
failure disables the mirror, so its worker cannot backpressure extension
draining or protocol progress. It writes through an independent duplicate of
inherited stderr where descriptor duplication succeeds; setup failure disables
only mirroring. Mirror traffic can still consume shared fd-2 sink capacity, and
the existing synchronous harness tracing may block on a full sink under
[ARCH-logging-io-analysis](specs/ARCH-logging-io-analysis.md).
The mirror is default-off because custom extension stderr is
unredacted and journal readers are a wider audience. It never inspects
extension stdout/protocol, events, journals, debug JSONL, provider captures,
Configure payloads, or secrets by category.
The one-shot `--prompt-stdin` path checks inherited stdout and stderr
independently. It sanitizes dynamic answer text only when stdout is a terminal,
and sanitizes dynamic reasoning, role, and `PromptStdinError` bodies only when
stderr is a terminal. Pipes and files preserve semantic UTF-8 body bytes inside
the existing framing; that raw nonterminal output is not terminal-safe and
should not be forwarded to a terminal without equivalent sanitization.

When changing one-shot rendering or terminal detection, re-check final-item and
streaming-fallback output, role and error bodies, crossed stdout/stderr terminal
policies, and exact nonterminal body bytes, prefixes, separators, and trailing
newlines.
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
The built-in Swarm `task_blocker` tool has a narrower presentation boundary: the
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
Interactive prompt-latency traces additionally use one wrapping fixed-size,
process-local submission sequence and fixed prompt-traffic classes. That
sequence is diagnostic correlation only: it never enters protocol messages,
events, journals, settings, or persistence. These records must never include
prompt or payload bodies, paths, credentials, durable observation identifiers,
or protocol correlation identifiers.
Opt-in trace logging may also carry bounded, content-free selected-presentation
facts from CLI-local socket delivery through redraw generation and successful
writer flush. It retains at most 64 exact facts per pass; additional facts
become count-only overflow. The allowlist is exactly
`agent.prompt_queued/prompt_queued`,
`agent.prompt_submitted/prompt_submitted`,
`agent.prompt_steered/prompt_steered`,
`provider.response_updated/response_updated`,
`provider.response_finished/response_finished`, and
`agent.prompt_terminated/prompt_terminated`. Its finite field inventory is
delivery id, that invariant label, wrapping mutation/frame generation,
monotonic duration, omitted/indeterminate count, and output failure kind/stage.
Counters saturate where they represent omitted work and wrap where they are
process-local generations. The existing tracing subscriber is a
transport-neutral operational diagnostic exception: an interactive UI currently
writes enabled records to its `ui.log`, while other entry points may use stderr
or a sink. These records do not enter event journals, settings, protocol
messages, replay state, or semantic persistence and have no durability
guarantee. The UI writer keeps one line-buffered descriptor behind a mutex; it
drains complete lines into the OS cache but does not call `File::flush`,
`fsync`, `fdatasync`, `sync_all`, or `sync_data`, and shutdown performs no
explicit drain.
The CLI socket-to-renderer FIFO is bounded at 1,024 items and 64 MiB, but
backpressure can migrate backlog to the harness writer queue. These bounds
therefore do not promise whole-process or end-to-end slow-client memory limits.

## Command-mode and prompt boundary

For interactive input, first-non-whitespace `:` selects non-provider command
authority. Unknown or malformed colon commands fail locally, while slash-prefixed
text—including obsolete command spellings—is ordinary provider input. A doubled
`::` escape keeps a typed literal marker through every harness-owned command
consumer even though history, durable prompt text, and provider projection contain
only the canonical single-colon text. This prevents canonical `:skill` prompt
text from being reinterpreted after the CLI removes the escape. `--prompt-stdin`
instead preserves its complete stdin text and marks it literal, so its initial
agent prompt bypasses CLI commands/actions and harness skill expansion.
The Gmail OAuth finish exception still applies after interactive literal
canonicalization: `::email auth google finish ...` becomes the fixed redacted
literal and never sends its code or state as a model prompt.

Only attached socket UIs may send `ui.create_agent`. The harness returns its
bounded, sanitized admission result directly to that live connection without
publication or replay. Distinct bounded request and prompt correlation ids keep
creation admission separate from later prompt processing. Pre-materialization
prompt failures publish only bounded sanitized diagnostics and correlation
metadata as transient `agent.prompt_failed` terminals; canonical provider
failures retain their existing prompt-id lifecycle. See
[SPEC-ui-create-agent-admission](specs/SPEC-ui-create-agent-admission.md).

The active editor temporarily contains Gmail OAuth finish arguments while the
user types them. After submission, they remain raw only on the immediate
parse/routing stack and in the single successful `ActionInvoke` delivered to
the exact owning PIM extension. The CLI uses exactly `:email auth google finish
<redacted>` for command echo, in-process and persistent history,
external-editor context, and content-enabled `ui.prompt_draft` publication.
Content-free drafts remain content-free. The harness excludes transient inbound
invokes from debug JSONL and redacts the published debug-log copy. Re-check stale
user-facing command instructions, interactive/headless parity, completion
precedence, literal escape handling around skills, and both debug-log paths
whenever command routing or action logging changes. See
[GATE-colon-command-mode](specs/GATE-colon-command-mode.md),
[SPEC-tau-cli-command-mode](crates/tau-cli/specs/SPEC-tau-cli-command-mode.md),
and
[SPEC-tau-harness-session-state](crates/tau-harness/specs/SPEC-tau-harness-session-state.md).
This exact-owner rule governs recognized interactive actions. `--prompt-stdin`
remains an explicitly literal model-prompt interface and does not invoke this
OAuth action classifier.

Peer harness messaging is cooperative same-UID local IPC, not a hostile-process
sandbox or per-sender ACL. Harness instances for one user are mutually trusted
and may deliberately connect to each other's ordinary UI sockets. Together they
form the harness side of the boundary that hides state and runtime sockets from
agent-controlled configured components; Tau does not claim procfs, ptrace, or
hostile same-UID containment. Harness-owned cross-session messaging retains
runtime socket discovery. A dedicated external-message connection is admitted
only to its message, callback-authentication, and session-probe RPCs and cannot
reach UI or Action handlers. Callback correlation prevents accidental sender/route
confusion before bounded admission or model-spending auto-start, while peer text
remains model input rather than a harness instruction. An authenticated bare-peer
auto-start immediately receives the target's configured role- and policy-filtered
tool surface so cooperative handovers can operate without a second UI prompt;
peer payload cannot select or expand that authority and retains extension
provenance. The hot dispatch and cold marker behavior are governed by
[SPEC-tau-harness-prompt-dispatch](crates/tau-harness/specs/SPEC-tau-harness-prompt-dispatch.md).
Delivery is best-effort at-least-once: an ambiguous crash or retry can duplicate
receive occurrences, agents, model work, and spend. Each accepted directional
occurrence is its owning journal's sole canonical payload projection. Local
inbound provider context exact-close-frames peer text inside a sender-labelled
wrapper; live activation uses a payload-free sequence wake, and replay restores
context without waking.
The target's complete foreground framed write remains ACK authority; ACK does
not wait for background filesystem sync. An ACK or provider effect can therefore
survive a crash that loses its journal fact. Tau adds no restart deduplication,
distributed WAL, or cross-journal transaction. See
[SPEC-semantic-journal-writeback-durability](specs/SPEC-semantic-journal-writeback-durability.md).

## Agent journals and summary checkpoints

`tau dev print-prompt` and `print-tools` keep session and preview-agent semantic
state, journals, transcripts, debug artifacts, and retention state process-local
or omit them, so they create no resumable session or agent and do not create or
open the durable agent store. They configure ordinary extensions and therefore
preserve User, Cache, Secret, direct-state, filesystem, network, and
external-service reads and writes. They load one fresh ephemeral agent through
bounded context readiness, resolve an effective model/tool snapshot, and never
call a provider. `print-system-prompt` retains the separate immutable MemoryOnly
storage policy. Each preview's unique runtime socket and discovery metadata may
exist only while its owned daemon runs; handled diagnostic exits remove that
exact pair after child reap, including forced-exit fallback.

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

The opted-in generic local compactor reuses the ordinary provider request
prefix for the immutable cut, including its system prompt, tools, messages,
images, raw tool-call arguments, and cache controls, then appends a
harness-authored `<tau_internal>` user message that forbids tool use and requests
only a summary. This deliberately does not create a separate compactor authority
or privacy-reduced transcript. The configured provider has already received the
same ordinary context; cache alignment avoids a second cold prefill when the
provider cache retains that prefix.

The extension rejects any tool call or other semantic output without execution,
accepts exactly one nonempty bounded assistant final text, and discards reasoning
and opaque replay items. The harness persists that text exactly once as one
synthetic user-role checkpoint, with no deterministic supplement or wrapper.
Before that terminal validation, standalone local-summary attempts publish only
the existing bounded content-free response byte/timing statistics and
status/activity signals at their existing cadence. Assistant text, reasoning, tool, and opaque output do
not cross in transient updates; invalid and canceled attempts publish no
content-bearing update. The validated narrative crosses once in the private
terminal envelope. Ordinary inference streaming and opted-in private provider
debug capture are unchanged.
Events committed after the immutable cut remain suffix history. Ordinary
opted-in provider debug captures apply to compaction; they are sensitive,
best-effort observability artifacts and never journal, transcript, replay, or
recovery authority.

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
[SPEC-provider-prompt-materialization-authority](specs/SPEC-provider-prompt-materialization-authority.md)
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
Startup retention uses one ordered detached worker and one wall-clock snapshot.
It finalizes committed detached session and agent trees, removes expired
unlocked sessions, strictly derives every durable agent ever loaded by each
surviving canonical session, removes only exact old unreferenced agents after a
durable permanent ID tombstone, then removes expired session `events.jsonl`
regular files and exact compressed `.json.zst` provider request/response
captures. Session and agent deletion are disabled by default; diagnostics
default to thirty days. Manifest or journal I/O uncertainty aborts agent
deletion; missing or malformed manifests remain noncanonical. Reference
discovery streams bounded records and reuses validated journal boundaries so
candidate rescans consume only appended suffixes. Locked, stale, future,
corrupt, replaced, or symlinked artifacts remain untouched. Atomic detach is
logical deletion, but recursive removal starts only after synchronizing the
staging parent before the canonical source parent. Completed removal
synchronizes staging again, and restart finalization first re-establishes the
same durable boundary. Any session finalization or cleanup failure suppresses
agent eligibility deletion for that pass, because an incompletely committed
session detach may return after a crash. Focused filesystem, reference,
tombstone, and ordering oracles live beside `session_cleanup`, `agent_cleanup`,
`retention_cleanup`, and `diagnostic_cleanup`.

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
The harness validates the closed phase, including self-resolving Waiting versus
intervention-required Blocked, and canonical 160-byte, single-line title
at both tool and durable boundaries. A challenged successful response remains
permanently withheld from watch, delegated-result, and detach projection even
though its semantic append remains in the transcript; post-commit handling only
queues guidance. The same guard applies while status remains Unreported only
when the immutable dispatched prompt surface exposed model-visible `status`.
After an accepted Waiting, Done, or Blocked transition, a distinct later successful final
may project and complete the outer turn, but only after its own exact append
commits. Each unresolved phase has its own bounded escape: it challenges at most
two successful finals, and the third within that phase projects even if status
remains unresolved. Entering Working resets the budget even after Unreported
challenges in the same outer turn. Escape invalidates Working to Unknown but
leaves Unreported unchanged. Prompts without `status` bypass this guard.
Waiting, Done, Blocked, an unsuccessful terminal that invalidates Working to Unknown,
budget escape, unload, and final shutdown release that runtime ownership;
append failure and interception rejection cannot project a challenged candidate. Work-status state
tests, harness gated-final/interception tests, and the deterministic current-status
provider scenario cover these boundaries. Revisit them whenever internal-tool
ownership, response interception, final projection, or agent teardown changes.
Repeated activating-input wait protection derives only from typed timeout
settlements at terminal publication. Focused supplied-clock tests cover ordinary
and compaction-rollback timeout publication, one-shot and Waiting suppression,
substantive-admission reset even without `status`, and non-timeout wait
exclusion. Revisit these oracles when settlement correlation, compaction claims,
tool admission, or work-status phase handling changes.

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
Only an attached socket UI may send the payload-free `ui_shutdown_request` that
unconditionally enters canonical harness shutdown. Other socket peers,
embedded/non-socket UIs, dedicated external-message peers, and configured
extensions are silently denied; they cannot stop the session. The request does
not become a published or persisted event.
Only an attached socket UI may send `ui_tree_request` and inspect agent prompt
anchors/previews. The harness returns exactly one requester-directed multiline
notice and does not publish the request or result. Other client origins and
configured extensions are silently denied; extension attempts retain normal
phase validation and metering but are denied before activation staging.
Tree prompt previews are untrusted terminal text. The harness encodes them
before constructing the common requester-directed notice, so interactive and
headless clients receive identical terminal-inert text without TTY-dependent
rewriting. Focused harness and CLI regressions protect the encoding, directed
routing, interactive VT result, and exact headless bytes. Security review must
revisit this boundary when changing the preview window or encoding,
`requires_visible_escape`, directed routing, or either client's tree rendering.
See
[`SPEC-tau-harness-session-state`](crates/tau-harness/specs/SPEC-tau-harness-session-state.md).
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
and a final-shutdown race can lose a record; ephemeral sessions intentionally
use the same durable file. These
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
cannot release session readiness or another agent. A connected effective
per-agent waiter has no deadline and can hold initialization until
acknowledgement or disconnect.
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
and child-creation logic. Accepted startup is bounded to 64 concurrent operations
and 4 MiB of aggregate retained instruction, span, task, and routing data; agent-id
reservations and requester query ids are bounded before storage side effects.
Each successful phase owns one ordinary committed or rejected publication outcome.
After acceptance, exactly one compact `agent.start_failed` terminal owns failure
projection and runtime cleanup; no cross-event reservation or journal transaction
is implied.

Clean process shutdown terminalizes every accepted start before revoking
admission and rejects a still-uncommitted acceptance without inventing a failure
fact. A hard crash can leave only the already committed durable prefix.
Cold restore keeps that prefix inspectable but never reconstructs requester routes,
buffered wakes, coordinator state, or initial-prompt dispatch. Only a committed
startup inference checkpoint permits ordinary post-start inference recovery.
Stream-local terminal persistence diagnostics target the exact agent generation;
whole-owner exit affects only durable starts bound to that current owner, not
memory-only or ephemeral starts. Retryable open, lock, write, and sync diagnostics
do not independently terminalize startup.

Unconfigured/socket peers are denied; stale generations or sessions are
observation-only, and raw requests never enter semantic replay. Deterministic
startup regressions preserve every pre/post-accept interception cut, canonical
prompt replacement, dispatch rejection, persistence-owner exit, cancellation and
shutdown boundary, terminal retry, bounded-index cleanup, and cold-restart prefix.
Revisit these safeguards when changing event interception, semantic persistence
generation ownership, agent-id templates, prompt dispatch, session shutdown, or
restore activation. See
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
hardening. Standalone-attempt accounting is a separate harness-sourced durable
fact: it carries required session correlation and normalized usage/rates/cost but
no credential or provider account identity. Its retry authority is bounded to 64
attempts, reserving attempt 65 for the terminal; larger configured-Provider
statuses remain transient diagnostics and cannot drive accounting or watcher
attempt state. A post-dispatch cancellation first publishes an Unknown awaiting
observation. Only the same live provider generation can publish its one durable
terminal correction; restart preserves the observation but restores no
correction authority. Provider disconnect, graceful shutdown, and agent unload
instead close still-dispatched owners as Final Unknown before discarding their
routes. See
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
Each durable session maps to one deterministic full-BLAKE3 lock basename and
socket basename. The daemon holds the lifetime `flock` before binding the socket
and keeps it through transport and durable-store teardown. The lock winner may
reclaim that session's stale socket; scanners never unlink it. PIDs are
diagnostic only and do not participate in lookup, routing, or liveness.
Targeted lookup bounds raw traversal and total time and fails closed when an
exact admitted connection cannot be proven.
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
tool-result strings. The extension places optionally authenticated Exa/Parallel
search/fetch, anonymous or optionally authenticated You.com search,
credentialed Brave search, and credentialed Tavily/Firecrawl search/fetch
results inside one exact-close-framed `<tau_web_content>` boundary with closed
adapter, operation, and external-trust labels, and enforces its result bound
after framing and closure. Exa, Parallel, and You.com retain anonymous paths;
configured You.com authentication selects its authenticated endpoint by
default. Credentialed adapters resolve API keys only from named Tau secrets and
send them in provider authentication headers; logs and model-visible errors
redact credential and endpoint material. Adapter
identity authenticates neither page authorship nor truth; provider titles,
URLs, ranks, sources, and prose remain untrusted body claims capable of prompt
injection. The envelope prevents exact closing-sentinel breakout but is not a
sandbox or instruction-authority change. See
[`SPEC-tau-ext-websearch-provider-boundary`](crates/tau-ext-websearch/specs/SPEC-tau-ext-websearch-provider-boundary.md)
and
[`SPEC-tau-ext-websearch-runtime-safeguards`](crates/tau-ext-websearch/specs/SPEC-tau-ext-websearch-runtime-safeguards.md).
Hybrid calls can send the same query or URL to as many as three configured
hosted providers sequentially, so one call has multiple external recipients and
each issued attempt can consume quota. Cancellation prevents later attempts,
but the blocking HTTP transport may retain an already issued request until its
admission-anchored deadline slice ends.

The disabled-by-default `std-slack` bridge is the separately installed
`tau-ext-slack` executable, supervised through Tau's normal trusted same-user
stdio extension route. It requires exact configured conversation/kind/thread
policy and verified live-human admission. Receive permission creates only
Tau-issued source-bound reply authority; proactive permission is a separate
alias-only grant. Dynamic DMs remain bounded, allowlist/exact-user-bound, and
reply-only.
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
Use one Slack extension instance for one receiving agent. The separately
maintained `tau-ext-slack` project owns the detailed architecture and routing
contract.
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
The separately maintained `tau-ext-slack` project owns its Slack-specific
review triggers, failure/replay invariants, operational guidance, and tests.

The disabled-by-default `std-telegram` bridge is the separately installed
`tau-ext-telegram` executable, supervised through the same trusted same-user
stdio extension route. Local-poll mode admits only configured numeric users and
one configured or allowlisted-user-linked chat; registration grants routes only
to explicitly registered loaded agents, and sending never accepts a
model-chosen native destination. Gateway-client mode gives the sidecar only its
configured socket endpoint and per-instance authentication secret; the
separately supervised `tau-telegram-gateway` retains bot-token, polling,
allowlist, chat, durable checkpoint, and outbound-routing authority. The
standalone project owns Telegram-specific review triggers, replay and
durability invariants, operational guidance, and tests.

The disabled-by-default `std-zulip` bridge is the separately installed
`tau-ext-zulip` executable, supervised through Tau's normal trusted same-user
stdio extension route. It uses bot HTTP Basic authentication and native
event-queue long polling. Exact numeric sender allowlists and configured DM or
stream/topic policies gate ingress; separately configured proactive-DM aliases
fix outbound recipient authority. Queue identifiers and native participant,
stream, topic, and message routes remain extension-local authority; model tools
accept only current configured aliases or bounded Tau-issued source references.
Opt-in send-only mode declares only the send tool for one fixed proactive-DM
alias and rejects every inbound route, allowlist, queue, reply, reaction, and
catch-up surface. With the default
`offline_message_catch_up: false`, queue expiry
reconnects at a fresh live tip and warns about a possible gap rather than
fetching missed backlog. Opting into `offline_message_catch_up: true` uses the
extension-local `CheckpointRuntime` to recover bounded newly created messages
after its durable position. Successful sends report before
their tool result, while ambiguous provider outcomes are not automatically
retried. The separately maintained `tau-ext-zulip` project owns its detailed
architecture, routing, testing, and security documentation.

## Standalone compaction recovery reliability

Standalone compaction and its continuation are harness-owned durable work. Every
new provider cut must be a closed transcript prefix; a tool-calling assistant
response and its complete terminal results node are indivisible. Native Codex
transient standalone failures use the shared scheduler for at most five total
attempts only before semantic compact output is accepted. An error processed
first in an event accepts no content and remains retryable. Once compact output
is accepted, any later failure discards that uncommitted output and terminalizes
without automatic retry; recovery requires a distinct explicit request.
Deterministic failures terminalize immediately. After terminal continuation or
background completion, ordinary input proceeds, while durable failure history
suppresses automatic threshold, policy, continuation, and reactive recovery for
the same provider-qualified model and branch. Model or branch drift permits
fresh independent work; returning restores suppression until a successful
matching explicit successor clears the failed chain.

An explicit successor may retreat its cut to retain more exact suffix, but it
must preserve same-branch coverage of any resume watermark and cannot replace
that watermark with an ancestor or sibling selected by later head navigation.
Ordinary input and `:cancel` do not clear durable failure authority. Core
validation and warm/cold exact-once replay regressions prevent duplicate provider
dispatch, terminal delivery, and continuation checkpoints.
The production-path safeguards
`compact_pre_progress_failure_remains_retryable`,
`compact_same_event_error_first_remains_retryable`,
`compact_post_progress_failure_is_terminal`,
`compact_exact_success_returns_one_item`, and
`compact_explicit_new_request_dispatches_after_post_progress_failure` preserve
the native Codex retry/cost boundary.
Revisit them when adding any explicit abandon/rewind operation or changing
compaction replay ownership, retry limits, failure classification, parser
precedence, semantic-progress correlation, pool repair, compact scheduler
mapping, or automatic suppression qualification.

Exact committed publication envelopes create or transfer activation ownership.
A retained completion envelope or standalone `AwaitingCheckpoint` tuple represents
durable work and remains bound to its owning branch. Queue/in-flight attempt
markers are ephemeral: every prevalidation or persistence rejection clears them,
and agent unload or final shutdown discards all warm-process retry state.
Transaction-owned publications carry their enqueue-time session generation and
must still match an exact live runtime owner before commit. Destructive lifecycle
cancellation suspends that interceptor's registration until its one outstanding
uncorrelated stale reply is consumed, so the reply cannot bind to later work
without changing the extension connection lifecycle. Registration
replacement remains suspended, no timeout applies, exactly one reply is consumed,
and disconnect clears suspension. The interface contract is specified by
[`SPEC-tau-harness-event-processing`](crates/tau-harness/specs/SPEC-tau-harness-event-processing.md).
Unrelated accepted publications retain FIFO order and complete or fail through
their normal path.
Final shutdown advances the session admission generation before quiescence. Raw
session-bound events whose contracts require observation may still commit, but a
central post-commit peer guard suppresses their semantic effects and releases
activation reservations. Process-global tool/prompt-fragment/model declarations
and provider-quota current-state reports may finish only while exact captured
connection-generation identity remains current.

The model-callable self `compact` capability is enabled by default and can act
only on the calling agent. Effective role policy may revoke it by exact tool
name, compaction group, or matching tag. The cross-agent `agent_compact`
capability remains independently disabled by default; explicitly granting it
authorizes compaction of another loaded same-session agent but does not alter
self-compaction policy.

V1-marked ordinary-inference cancellation journals the exact matching
`AgentPromptTerminated(Canceled)` before it releases placement ownership. Cold
replay folds that private closure and rejects a late response, subject to the
existing asynchronous-writeback loss boundary. Legacy and unmarked ordinary
termination remains transient. Standalone-compaction ownership remains covered by
the separate durable rules above. See
[`SPEC-agent-message-delivery`](specs/SPEC-agent-message-delivery.md) and
[`GATE-asynchronous-journal-durability`](specs/GATE-asynchronous-journal-durability.md).

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
classification is forgotten on unload or process exit; cold
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
stdin/stdout and runtime, checks foreground restoration after settling the child,
and revalidates the selected agent. Restoration failure preserves the primary
picker outcome and the restoration error; Drop retries only as best-effort
cleanup. When foreground ownership remains unconfirmed, Tau does not resume raw
input, redraw, or terminal cleanup, disconnects only that UI attachment, and
leaves an owned harness daemon running. Checked restoration retries only `EINTR`,
binds handoff and restoration to Tau's actual process group, rejects an initial
foreground mismatch, accepts only that actual group as already restored, and
otherwise retains the fail-stop. Before teardown, the private UI log records one fixed restoration
class and optional numeric errno for non-ephemeral UIs; failures without a
syscall errno use the exact bounded value `restoration_errno=none`. Ephemeral
mode retains its no-artifact sink behavior. Neither field enters protocol, replay, or semantic
persistence. Cancellation, ordinary subprocess/RPC
errors after confirmed restoration, and stale selection are no-mutation outcomes.
Focused safeguards cover bounded settlement and group cleanup, the high-level
no-resume guard, attachment-fatal routing, and raw-terminal paused Drop/no-write
behavior. Revisit them together when changing picker subprocess ownership,
terminal pause/resume, input-loop error routing, or daemon exit disposition.
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

`tau agent cache` and `tau session cache` perform offline inspection only. Their
content-free reports omit bodies, provider IDs, endpoint/account/cache-key values,
and source paths, but retain local navigation IDs and workload accounting. They
write no state or indexes, never contact providers, and do not enable captures.
Legacy private capture files provide only partial inventory evidence, not an
exhaustive attempt ledger or exact terminal joins. See
[`docs/agent-cache.md`](docs/agent-cache.md) for the initial coverage and
conservative resource-admission limits.

`tau agent trace` can export unredacted durable journals. Native, OTLP, and
compact output can contain full
prompts, reasoning, images, tool arguments and results, messages, model
parameters, usage, and cost data. Treat native, OTLP, compact JSONL, and compact
TOON output as sensitive as the original state directory; redirect or transmit
it only to trusted destinations. Compact lite mode exposes tool names, arguments,
commands, statuses, output sizes, and up to 4 KiB of unredacted normalized output
per terminal call, including bounded rendered error details. Full mode exposes
complete normalized output and rendered error details.

The exporter opens only existing state and never repairs or writes it. Only
writer-lock contention selects checkpoint mode. Inactive journals acquire their
shared locks before opening the journal and selecting EOF; lock-held journals
retain one exact opened journal identity and select a finite prefix through the
existing bound checkpoint. A small bounded retry tolerates only an atomic
journal/checkpoint replacement observation race. Bounded positional reads and strict replay validate
every selected prefix, descendant discovery is rechecked after capture, and all
failures occur before stdout. Capture never waits for lock release or includes
records committed after its selected cuts. A snapshot retains one journal file
descriptor per included agent plus lock descriptors for inactive agents until
private staging finishes. A very large selected workflow can exhaust process
file descriptors and fail before output.

`agent-performance-jsonl` is the preferred content-free audit projection, but it
still exposes identifiers, models, timing, usage/cost, membership, and work
patterns. Keep it private even though it omits prompt, tool, response, error, and
provider-body content.

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
estimated cost, call/observation/turn/transaction identities, and work patterns
remain sensitive metadata. It streams each event into a compact content-free
classification: per-occurrence identity/timestamp, declaration and canonical
terminal call identities, typed tool/wait/activation/outer-turn boundaries, and
standalone accounting counters. It drops provider payloads, tool names and
arguments/results, errors, model parameters, endpoint/backend/rate metadata, and
cumulative usage snapshots. Heap remains proportional to selected occurrence and
correlation cardinality plus retained identifier bytes, not selected journal
payload bytes. Changes must recheck zero and decreasing wall timestamps,
duplicate terminals, checked aggregate overflow, zero/fractional cache-ratio
boundaries, typed reference integrity, and structural output-field privacy.

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


## Output-length continuation failure boundary

`ProviderStopReason::Length` is never semantic success. Tau permits at most one
successor in an ordinary user outer turn, and only when the accepted response
contains only reasoning the selected adapter can replay exactly. Chat Completions
requires non-empty full reasoning; generic public Responses requires at least one
opaque reasoning item with no `encrypted_content`, an absent or empty `summary`,
and a non-empty `content` array consisting only of string `reasoning_text` parts.
It permits only the matching non-empty full display companion. Summary-bearing,
encrypted, empty, and mixed reasoning cannot acquire continuation authority.
Private ChatGPT/Codex Responses does not acquire this authority.

The durable authority chain is
`provider.response_finished(plan)` → exact trusted internal steer → matching
inference owner → `agent.prompt_started`. Each append must complete before its
successor begins. After the owner commits, restart never reconstructs or resends
the provider request. A successor terminal may repair one missing settled
outer-turn finish only when its validated `outer_turn_finish_owed` bit says so.

Cancellation, branch-lineage loss, and missing or changed logical model routes
fail closed. They neither redirect retained reasoning nor reset the one-shot
budget. Branch loss closes the dormant original lineage with its exact steer,
owner, pre-start failure, and owed finish; it never dispatches the successor or
folds those facts into the selected sibling. A reserved successor's reactive
context recovery carries only recovery authority, while its exact
transaction-owned descendant retains the spent output-length lineage.
This dormant synthetic failure is authorized only before the reserved
successor's durable prompt-start. After prompt-start, branch movement cannot mint
a competing failure; the already-dispatched owner remains the sole terminal
authority.

Each accepted response keeps its own usage, cost increment, and harness-derived
finite provider attempt, including the incomplete source and successor. Cold
recovery may repair the plan-to-steer and steer-to-owner cuts, reconstruct sticky
terminal-incomplete attempt state, and apply the separately stamped
terminal-to-finish cut above. Malformed, duplicate, or unrelated lineage facts
block without dispatch. Revisit this boundary before adding
provider-native continuation, more than one successor, unchanged-request retry,
or any broader replay capability.

## Tau Swarm extension

`std-swarm` is a configured local extension that connects to one
cryptographically pinned Iroh endpoint. The configured credential is sensitive;
Tau supplies it only through the declared Configure secret and the extension
must not log it. Relay and direct addresses are reachability hints and do not
weaken endpoint identity verification.

Remote prompts and blocker answers reach agents only through Tau's canonical
internal-prompt path. The extension retains command deduplication, blocker
history, unacknowledged updates, and replaceable current task metadata in process
memory under configured bounds. Task metadata accepts at most 4,096 current
entries and 8 MiB of aggregate canonical task-ID, title, and description bytes;
the authenticated Swarm peer independently enforces the same ceilings.
Task IDs have no ownership model. Ordinary Tau role tool policy is the whole
grant boundary: any loaded agent granted `task_info` may replace metadata for
any valid task ID in its current session. Revisit this authority boundary if
task ownership or narrower grants are introduced.
Tau Swarm binds commands and active lifecycle state to a collision-resistant
extension-process incarnation. Ordinary reconnects retain the process command
table; a replacement process declares a fresh incarnation,
so the server fences ambiguous old commands and supersedes old active lifecycle
state. A peer that sends many unique, otherwise valid commands can fill the
no-eviction command table and deny later remote commands until process restart. Large
configured bounds can exhaust extension memory; they are operator trust and
capacity choices rather than untrusted local-IPC hardening boundaries.

Ordinary Iroh reconnect retains current task metadata. An extension-process
restart loses it; the fresh process's complete snapshot
converges the peer by omitting the old incarnation's metadata. Revisit this
accepted non-durability if task metadata gains journaling, persistence, or
different session/process lifecycle ownership.

Each Swarm worker generation owns publication authority. Worker return or panic
unwind retires that authority synchronously before any optional terminal
notice; panic-abort builds terminate the extension process instead.
`task_info`, `task_blocker` add/cancel, and `task_update` serialize their full admission and mutation
sections against retirement; after retirement they fail before changing
process-memory state or reporting success. Revisit this synchronization when
changing worker lifecycle, mutating tool paths, health ownership, or terminal
notice ordering. Deterministic worker-return, panic, mutation-ordering,
tool-authority, saturation, and cleanup regressions protect this boundary.

### Compact semantic trace disclosure

Compact lite traces expose up to 4 KiB each of unredacted assistant prose, displayable reasoning, explicit sent/received message text, and tool output, in addition to complete tool arguments. Full mode exposes complete text and output. Reasoning and messages can contain secrets, private communications, user data, or model-derived sensitive content; paired directional records can duplicate the same sensitive body across included journals. Absolute timestamps, agent/session/message/prompt IDs, membership, and activity patterns are sensitive metadata. Cross-agent wall-clock order is not causality.

Compact identity, sequence, and timestamp fields remain facts of the captured
journals when those journals move between sessions; export never rebinds them to
the containing session. Original-host wall-clock samples can regress or differ
across hosts and do not establish delivery order, latency, or happens-before.
Tau keeps extension credentials in harness-mediated configured-instance Secret
scope. Supervised extensions run in mandatory fail-closed Linux user and mount
namespaces that hide the whole Tau secret root. Only configured Provider
instances receive credential-free provider settings from a bounded disjoint union
of XDG config and state. Config leaf symlinks may target bounded regular
Nix/dotfiles deployment files outside the canonical config instance root; broken,
non-regular, invalidly named, and oversized profiles fail closed. Mutable state
retains no-symlink restrictions. Persistent instances get an
ephemeral read-only materialization of the exact `Configure.settings_files`
snapshot, while memory-only previews get only that immutable snapshot. Tool instances receive
neither. This is defense in depth for trusted configured same-UID executables, not containment
from malicious same-UID code or misuse of credentials returned to an authorized
extension. Secret payloads remain absent from logs, events, journals, generic
debug output, and errors. See
[`SPEC-extension-secret-storage`](specs/SPEC-extension-secret-storage.md).
The separately installed `tau-ext-telegram` executable receives gateway-client
credentials through the same configured-instance secret mechanism. Its
separately installed `tau-telegram-gateway` peer uses that key for mutual local
protocol authentication. This removes ambient authority from socket-only peers,
but does not prevent malicious same-UID code from extracting an authorized
extension's in-memory key through `/proc`, ptrace, or direct memory access.
Named-secret environment discovery matches the exact `TAU_SECRET_` prefix in
the OS-native key representation and ignores unrelated raw entries. A matching
suffix or value that cannot enter the UTF-8 schema returns a typed,
value-redacted error. Harness source capture removes every matching native key
before returning either its snapshot or a source error; provider setup retains
the caller's entries. Every production supervised spawn and respawn
independently removes matching native keys from the child environment.

Changes to that predicate, disposition, spawn ordering, or error formatting
must recheck the Unix raw-entry regressions in `tau-config` secret-source tests
and the `tau-harness` extension and lifecycle tests.

### Model self-information disclosure

The default-enabled, policy-authorized `self_info` tool exposes the calling
model agent's agent/session identifiers, exact prompt-owned model and effort,
call-time work status, and—only in durable mode—the local session path. Its
seven-line response enters the transcript as an ordinary tool result and is
therefore persisted with that agent's normal durable history. Configured
extensions cannot invoke the tool directly; the harness requires a model-owned
call and correlates it with the invoking prompt-start fact.

Header values use a line-safe byte encoding: printable ASCII remains readable,
backslash is doubled, and controls, non-ASCII bytes, and invalid path bytes use
`\xNN`. This prevents model names or local paths from injecting apparent
headers while preserving exact path bytes. Revisit this disclosure boundary and
its focused regressions when changing tool policy, prompt/call correlation,
storage modes, exposed fields, or the header encoding.

Startup, setup inspection, and development-copy paths share the
4,096-profile-per-instance, 1-MiB-per-profile, and per-instance merged snapshot
byte limits. They validate the opened descriptor, using nonblocking Unix opens
so a raced special-file target cannot stall discovery.

Named provider API-key bindings use one closed parser shared by setup, harness,
and provider runtime. The harness consumes the configured declaration without
forwarding its value in `Configure.secrets`, refreshes only canonical API-key
records, and replaces unavailable bindings with empty typed records plus
value-redacted warnings. Cross-layer duplicates fail rather than overriding or
coalescing. Tau never locks an external config or symlink-target inode; a
per-instance private-state providers lock precedes the
Secret-scope lock in setup, removal, and startup, binding source selection and
credential publication to the exact retained Configure snapshot. Typed
Tau-component identity, rather than executable argv resemblance, grants this
authority. Source read/decoding failures preserve older records and fail or skip
the provider according to its required policy. Memory-only startup forwards no
built-in provider declaration values.

Model-facing generic user-payload framing follows [SPEC-exact-sentinel-prompt-envelopes](specs/SPEC-exact-sentinel-prompt-envelopes.md); payload-local XML-like tags do not establish Tau provenance.
