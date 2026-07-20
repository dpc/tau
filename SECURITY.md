# Security policy

Tau is early-stage software, but security issues are important. Please report suspected vulnerabilities through GitHub private vulnerability reporting for `dpc/tau` (<https://github.com/dpc/tau/security/advisories/new>) when available. If that path is unavailable, contact the maintainer privately first and avoid filing a public issue with exploit details.

For technical trust boundaries, start with [ARCH-external-message-boundary](specs/ARCH-external-message-boundary.md) and the applicable project and component records under `specs/` and `crates/*/specs/`.
The distinct deterministic and live/VCR test-fixture boundaries are documented
in [`tau-e2e-tests/SECURITY.md`](crates/tau-e2e-tests/SECURITY.md).
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

Peer harness messaging is cooperative same-UID local IPC, not a hostile-process
sandbox or per-sender ACL. Callback correlation prevents accidental sender/route
confusion before bounded admission or model-spending auto-start, while peer text
remains model input rather than a harness instruction. Delivery is best-effort
at-least-once: an ambiguous crash or retry can duplicate prompts, agents, model
work, and spend.

## Agent journals and summary checkpoints

Per-agent `events.cbor` journals are authoritative durable identity and
transcript state. Their `meta.json` files are content-minimized, atomically
replaced derived checkpoints, not routing authority. Checkpoints bind an exact
frame boundary and sequence to journal file identity and a boundary witness;
stale or invalid checkpoints are repaired only under nonblocking byte, record,
and time budgets. Metadata-only, empty, corrupt, or otherwise unvalidated
artifacts reserve ids but cannot receive routed facts.

Summary files intentionally omit prompt previews. Legacy preview-bearing
sidecars are unverified hints and are scrubbed when strict journal migration can
acquire the agent lock. Repair never rewrites or salvages a journal, and failure
to publish derived metadata does not invalidate an already committed record.
This is cooperative same-UID crash consistency, not tamper detection: arbitrary
same-inode/same-size journal mutation is outside the append-only store contract.

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
[`DECISION-agent-watch-acyclic-topology`](specs/DECISION-agent-watch-acyclic-topology.md).

## Local IPC and external ingress

Configured extension processes are trusted local executables. “Less-trusted
extension” means protocol authority is limited—the harness still validates phase,
source ownership, routing identity, configuration, and collisions—not that the
stdio stream is a hostile availability boundary or process sandbox. Operation
quotas do not promise to bound protocol deserialization; see
[`SPEC-tau-harness-session-state`](crates/tau-harness/specs/SPEC-tau-harness-session-state.md#extension-data)
and [`ARCH-tau-supervisor`](crates/tau-supervisor/specs/ARCH-tau-supervisor.md#child-environment).
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
Checking a metadata PID, socket reachability, and pathname identity cannot be
atomic with PID reuse and a daemon replacing that pathname, so scanners must
not unlink apparently stale lifecycle pairs. Owned CLI shutdown closes the
initial transport first so the daemon normally removes its own pair, with
bounded forced termination retained as a last-resort availability safeguard.
Targeted session lookup bounds raw traversal, matching candidates, metadata
bytes, and total time and fails closed when uniqueness remains unproven,
including unreadable conventional metadata owned by a live or
liveness-unknown PID.
Local running-session listing isolates bounded raw traversal from its caller and
uses runtime paths only as socket candidates. Each responsive daemon returns its
in-memory current session id through a correlation-matched, per-probe-deadline
local socket RPC; adjacent metadata and persisted session directories are not
lifecycle authority. The CLI escapes record separators and terminal controls
before writing line-oriented output.

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
text inside one escaped `<tau_web_content>` boundary with closed adapter,
operation, and external-trust labels, and enforces its result bound after
escaping and closure. Adapter identity authenticates neither page authorship nor
truth; provider titles, URLs, ranks, sources, and prose remain untrusted body
claims capable of prompt injection. The envelope prevents markup spoofing but is
not a sandbox or instruction-authority change. See
[`SPEC-tau-ext-websearch-provider-boundary`](crates/tau-ext-websearch/specs/SPEC-tau-ext-websearch-provider-boundary.md)
and
[`SPEC-tau-ext-websearch-runtime-safeguards`](crates/tau-ext-websearch/specs/SPEC-tau-ext-websearch-runtime-safeguards.md).

The Slack bridge requires exact configured conversation/kind/thread policy and
verified live-human admission. Receive permission creates only Tau-issued
source-bound reply authority; proactive permission is a separate alias-only
grant. Dynamic DMs remain bounded, allowlist/exact-user-bound, and reply-only.
Slack submits transient message reports through ordinary interception; the
harness later publishes immutable canonical facts. Actionable reply and reaction
authority stays in extension-local runtime state. The Slack extension drops
recently repeated native occurrence ids with a bounded process-local cache before
report submission. Generic event infrastructure does no native deduplication or
ownership resolution: each canonical fact is a new immutable occurrence.
Cache eviction, restart, or races may therefore duplicate delivery.
Slack records an occurrence before identity lookup, local effects, capacity
admission, and local report write; a later transient failure consumes that
occurrence until eviction or restart rather than retrying it.
Use one Slack extension instance for one receiving agent as specified by
[`DECISION-tau-ext-slack-single-agent-operating-model`](crates/tau-ext-slack/specs/DECISION-tau-ext-slack-single-agent-operating-model.md).
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

## Standalone compaction recovery reliability

Standalone compaction and its continuation are harness-owned durable work. Every
new provider cut must be a closed transcript prefix; a tool-calling assistant
response and its complete terminal results node are indivisible. A failed
transaction with a resume watermark remains fail-closed until an explicit
successor preserves same-branch coverage of that watermark. A successor may
retreat its cut to retain more exact suffix, but it must not replace the owed
watermark with an ancestor or sibling selected by later head navigation.
Ordinary input and `/cancel` do not abandon this ownership; if the selected head
no longer descends from the owed watermark, explicit recovery must remain
blocked. Core validation and warm/cold replay regressions enforce these rules.
Revisit them when adding any explicit abandon/rewind operation or changing
compaction replay ownership.

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
The harness accepts absolute mutations only through UI intake; extensions cannot
mutate them. Modes do not authorize loading, routing, prompt delivery, watches,
execution, or model access and are intentionally not durable.

The directed agent-roster RPC is available only to same-user local connections
classified as UI clients. It exposes stable ids, lifecycle/persistence,
navigation/runtime status, creation role/parent/time, and a verified display
name, including unloaded history when requested. These are content-minimized
coordination labels, not secrets or an authorization boundary. Results go only to
the requester and never enter event publication, interception, subscription
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
