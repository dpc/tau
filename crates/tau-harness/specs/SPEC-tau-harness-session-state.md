# SPEC-tau-harness-session-state: Session State

## Record justification

Session state spans durable session and agent stores, harness cold restoration,
runtime routing and navigation classification, UI roster projection, and
extension-owned state, so no component-local owner can describe their shared
lifecycle and recovery invariants coherently.

## Session and agent stores

The session store owns first-transition durable membership facts such as
`session.agent_loaded` and `session.agent_unloaded`. Restored initialization
restates already-folded loaded membership transiently with a fresh initialization
ID rather than appending a duplicate membership fact. `session.started` and
`session.shutdown` are must-pass, immutable runtime/current-session snapshot
facts, but they are not folded into the durable session membership store. Agent
stores own durable transcript facts, including `agent.started`, prompt facts,
provider/tool results, harness-owned inter-agent message projections, and
per-agent metadata set/unset facts. Metadata is committed through the same
interceptable publish path as other ordinary events; the folded latest metadata
snapshot is replayed to subscribers before `session.agent_loaded`, and
inheritable entries are copied to child agents when an explicit or derived
parent is known. Tests should assert durable stores, not only runtime delivery,
when changing durable facts.

Cold resume acquires the selected session's existing lock without creating a
directory or lock file, then revalidates valid persisted metadata while the lock
remains held. A target deleted after CLI selection therefore fails startup
instead of being recreated as an empty session. New-session startup retains the
separate creating lock path.

Agent stores also fold the latest `agent.initialization_context_set` as
replaceable side state. It carries frozen effective skills and the optional
rendered AGENTS.md bootstrap block but creates no transcript node and does not
advance the branch head. Cold resume refreshes every loaded agent before prompt
dispatch, and every finalization records its fresh process-unique initialization
ID as a durable replacement even when effective content is unchanged. Branching
and compaction still materialize the latest active bootstrap exactly once rather than
compacting stale ordinary history. See
[SPEC-session-discovery-declarations-and-readiness](../../../specs/SPEC-session-discovery-declarations-and-readiness.md).

Loading an existing durable agent into a session that has not previously
contained it queues a one-shot hidden notice for that agent's next user prompt.
The notice warns that session-scoped tool and extension state can differ and
calls out timers as setup that may need recreation. Cold session resume uses the
same guidance in its restore notice because runtime can stop before the
agent-specific notice is folded into a durable prompt. The harness caches
ever-loaded membership for the bound session, including memory-only ephemeral
membership, so repeated prompt routing does not rescan journals and same-session
unload/reload does not produce a false warning. After cold resume, a historically
loaded id without a current live or pending route is classified as stopped for
local and external exact-message errors rather than as an unknown id.

Ephemeral session mode (`tau --ephemeral`) replaces the session membership store
with an in-memory store for the current harness process and suppresses
session-owned disk artifacts: membership logs, metadata/locks, debug
`events.jsonl`, per-session stderr logs, and session-scoped extension data.
This does not make agents ephemeral: the global agent store remains durable, so
prompts, responses, tool results, metadata, and sub-agent transcripts keep their
normal persistence. User/cache extension data, provider state, credentials,
configuration files, and runtime sockets are also outside the session-ephemeral
boundary.

Memory-only harness mode is a separate immutable policy. It always starts a
fresh session and uses process-local session and agent stores; it never resumes,
recovers, repairs, migrates, locks, indexes, or cleans durable state. Session
and agent events retain their ordinary in-process folding, ordering, and replay
behavior, while journals, metadata, checkpoints, debug JSONL, provider
captures, logs, extension storage, and retention mutations remain disabled.
The owned runtime socket and metadata pair are the only harness-managed files
permitted during the process lifetime and are removed after every handled exit
once the child is reaped.

Agents can separately be staged as ephemeral from the TUI (`:new` then
`:ephemeral on`). That policy is per agent: the harness marks the agent id before
the first semantic write, stores its transcript and metadata in the live
`AgentStore` only, and folds its `session.agent_loaded` membership fact in memory
without appending it to a durable session journal. Late subscribers attached to
the same daemon replay those memory records, but cold resume sees only durable
agents. Children of ephemeral parents inherit the memory-only policy so delegated
work does not accidentally create durable child transcripts.

## Semantic store durability

Memory-only agent and session stores fold the same semantic facts as durable
stores for live and same-daemon replay while creating no reserved state
directories, sidecars, locks, or event files. Journal `events.cbor` replay
validates framing, monotonic durable sequence numbers, path-safe store IDs, and
the same semantic event/parent invariants as live append. Read-only inspection
remains strict. Recovery under the writer lock retains the longest valid prefix
and truncates the first corrupt, truncated, spliced, or semantically invalid
frame and every later byte, even if a later frame looks valid.

Journal-backed agent, ordinary-session, and session-restore appends capture the exact
journal EOF before writing a length-prefixed CBOR frame. Prefix, payload, or
payload-write failure rolls the journal back to that EOF before returning the
original append error. A live store that cannot truncate to the old EOF rejects
later appends to that journal without touching it. A complete frame immediately
advances sequence and folded state. A coalesced lifecycle-owned worker later
syncs journal data and newly created directory entries, retries failures, and
never blocks or retracts semantic acceptance. Recovery truncation is itself
marked dirty for background sync.

No durability barrier precedes provider, tool, renderer, or other external
effects. A crash may therefore preserve an external effect while losing its
journal fact. Process crash relies on ordinary kernel writeback; kernel or power
loss may lose or tear an unsynced suffix. This boundary is governed by
[SPEC-semantic-journal-writeback-durability](../../../specs/SPEC-semantic-journal-writeback-durability.md).

Durable sequence numbers count only records written to that stream. In a
durable session, memory-only ephemeral-agent membership is retained in a
separately sequenced process-local overlay. Late same-daemon replay validates
and folds the durable journal before composing the validated overlay; cached
membership never bypasses journal validation. Restart discards the overlay and
the corresponding ephemeral transcripts.

Only one real background completion is accepted for a globally unique tool-call
ID. Once a background result or error is recorded, later completions for that ID
are rejected during live append and replay. Duplicate detection is global, while
the known-call check remains branch-relative to the event's explicit fold parent
rather than the mutable tree head.

Unrouteable canonical external-message facts share the session stream with
membership records as specified by
[SPEC-external-message-reports-and-facts](../../../specs/SPEC-external-message-reports-and-facts.md).

The debug JSONL mirror is part of this boundary: content-bearing agent, prompt,
provider, tool, shell, or delegation events for ephemeral agents must be
classified before logging. New event kinds that carry agent transcript content or
reference prompt/tool-call ids must update that classifier and its regression
tests. Full `agent.prompt_created` observations are represented by a fixed-shape,
content-free summary of bounded identifiers and saturating counts; system text,
context items, tool schemas, and image bytes are never serialized into
`events.jsonl`.

Each debug JSON object and its trailing newline are serialized before immediate
nonblocking admission to one bounded process-wide FIFO. The detached writer
takes `<session>/events.jsonl.lock` separately for each line, seeks exact EOF,
appends and flushes, rolls a failed append back to the prior EOF, then releases
the lock. It never fsyncs. Lock/open/write work, worker shutdown, and queue
capacity never block harness event or lifecycle work. Overflow and recoverable
I/O failures may omit individual lines; uncertain rollback poisons the
process-wide writer. This non-authoritative diagnostic mirror does not promise
crash or power-loss durability: termination can lose queued lines or leave a
missing or torn final line, and restart neither repairs nor salvages that line.
At durable Tau startup, a detached best-effort cleanup pass removes expired
`events.jsonl` regular files and legacy `.json` or compressed `.json.zst`
request/response files directly below `debug/provider-requests`, according to
`diagnostic_retention_days` (fourteen days by default). It skips the current
and locked sessions, does not follow symlinks, and never removes canonical CBOR
journals, session directories, unrelated JSONL, or other debug files.

`tau_harness::debug_log_timing` separately traces producer
serialization/admission and worker I/O. Worker records include exact monotonic
microseconds for EOF lookup, write/flush, and rollback, plus line byte count,
start/end EOF, and result class. Cycles over 500 milliseconds emit the same
content-free fields at warning level. These diagnostics remain tracing output
only: they never write recursively to `events.jsonl` or alter queue, append,
flush, rollback, or failure behavior.

The debug JSONL mirror also has a narrow temporary redaction exception for
`action.invoke` events with action id `email.auth.google.finish`: the harness
redacts raw action arguments because the current action schema cannot mark the
pasted Gmail loopback URL as sensitive and that URL contains a one-time OAuth
authorization code. Routed `ActionInvoke` delivery still carries the raw
argument to the owning extension. Future schema/protocol sensitive-argument
metadata should replace this action-id-specific debug-log redaction.

## Extension data

Extension-data RPCs confine paths to per-extension state roots, reject traversal
and symlink escapes, write private files/directories where supported, and enforce
per-file/per-directory-list quotas. Quota failures are reported as
`quota_exceeded`. These limits bound individual harness operations, not aggregate
extension disk usage across many files, arbitrary extension code, or protocol
deserialization before operation validation. Extensions remain trusted to execute
on the host.

In ephemeral session mode, `ExtensionDataScope::Session` is rejected before any
session data root is created. `User` and `Cache` scopes remain durable because
they are extension-owned non-session storage.

In memory-only harness mode, Session, User, and Cache extension-data requests
all return Permission before resolving or creating any root.

Per-agent metadata is durable, extension-visible, and interceptable coordination
state rather than a secret store; key ownership is conventional and writers
remain subject to harness validation. Peers publish metadata mutation requests
with `persist=false`;
only the validated harness-authored canonical facts enter this durable
state. See
[SPEC-agent-metadata-requests-and-canonical-facts](../../../specs/SPEC-agent-metadata-requests-and-canonical-facts.md).

## Durable branch-head navigation

The harness presents `:tree` as prompt rewind anchors by default. Numeric
anchors are one-based user-facing prompt positions; resolving an anchor moves
the durable branch head to that prompt node's parent, so the next user prompt
replaces or branches before the selected prompt. Root/before-first navigation is
represented by an explicit durable root head, while raw transcript node
navigation is only accepted through the explicit debug node target.
Default anchors are derived from durable prompt provenance, not merely from the
folded `UserInput` node shape: visible user-originated `agent.prompt_submitted`
facts and visible queued-user `agent.prompt_steered` facts are anchors, while
injected user messages, internal prompts, compaction triggers, assistant/tool
nodes, and agent-message projections are not.
Anchor previews use the raw canonical accepted prompt text, never the
provider-only `<user>` projection described by
[SPEC-interactive-user-prompt-envelope](../../../specs/SPEC-interactive-user-prompt-envelope.md).

Live agent-message activation is runtime-only branch ownership. Navigation to a
sibling is allowed but cannot acknowledge or scan an owed wake from the other
branch; that wake remains dormant until its branch is reselected or endpoint
lifecycle cleanup retires it. See
[SPEC-agent-message-delivery](../../../specs/SPEC-agent-message-delivery.md).

`agent.head_moved` is durable cursor state, but it is not a permanent override
over later transcript records. Agent-log replay folds head moves and
node-producing events in chronological order; every later prompt/assistant/tool
node advances the folded `AgentTree::head()`. Resume therefore restores the
conversation cursor from the replayed tree head, preserving root head moves only
until a later branch-advancing event supersedes them.

Restored background-tool interruption notices are queued by session and owning
agent. They must be folded only into the next real user prompt for the agent
whose background call was repaired, so one loaded agent in a resumed session
cannot consume or see another agent's restored background-tool notice.

Cold restore closes each unresolved foreground tool call conservatively without
redispatch. It commits one durable agent-owned `provider.tool_error` with the
restart and possible-side-effect diagnostic, then derives one harness-authored
nonsemantic `tool.error`. The durable provider error balances the provider
context and makes later cold resumes idempotent; an already repaired call
receives no second pair. Canonical terminal publication follows
[SPEC-terminal-tool-reports-and-canonical-outcomes](../../../specs/SPEC-terminal-tool-reports-and-canonical-outcomes.md).

New durable agents commit `AgentStarted` as journal sequence zero before the
harness publishes their route or session membership. Loading an existing agent
into another session publishes membership but never appends a second creation
fact. A sidecar-only artifact reserves its id but is not a semantic routing
identity. See
[ARCH-tau-core](../../tau-core/specs/ARCH-tau-core.md).

## Navigation classification

The harness owns a daemon-lifetime mode for every loaded current-session agent.
UI disconnect preserves it; committed unload, session switch, and process exit
forget it. Cold restore recomputes ordinary/delegated defaults and does not
restore explicit overrides.

An authenticated visible human prompt accepted for an existing loaded target
performs an implicit absolute `active` write. Selection alone, rejected prompts,
internal or extension inputs, queue promotion, steering, and replay do not write.
The implicit value survives same-daemon disconnect/reconnect like an explicit
override, but unload or session switch clears it and cold restore recomputes the
ordinary/delegated default despite durable historical prompt facts.

A completed durable parented start-agent worker is an ordinary loaded, idle,
addressable conversation rather than work still owned by its transient request.
Warm completion detaches both tool-backed starts and explicit-parent typed
starts without a tool call. Cold restore converges on that state when the
terminal fact was persisted before detachment. Its immutable
`AgentStarted.parent_agent` creation fact supplies the delegated `active_auto`
default while historical prompt and response originators remain unchanged. A
fresh user turn cannot emit another start result or unload the worker as request
completion. Parentless non-tool typed starts remain one-shot and unloaded; peer
entrypoints retain their separate ordinary-agent lifecycle.

## Agent roster projection

The current roster scope contains every distinct currently loaded membership id.
A current member with a non-terminating runtime agent and navigation mode is
`live`; any other current member is `unavailable`. History scope adds every
distinct id with a prior load whose composed latest membership state is
unloaded. Durable session history survives restart; ephemeral history exists
only in the validated process-local overlay.

Projection uses committed loaded and ever-loaded caches seeded before runtime
restoration and updated after membership persistence. Restore/commit failure
invalidates the projection and fails later requests atomically. The fixed entry
limit is checked before retaining a result.
Per-agent enrichment reads only the bounded first creation record and an
already-loaded or journal-bound checkpoint display projection; missing, invalid,
and unreadable facts remain categorical rows. Any snapshot failure is atomic.
For live agents, the same event-loop snapshot copies the current harness-owned
work-status phase and canonical title; unavailable and unloaded rows omit it.
