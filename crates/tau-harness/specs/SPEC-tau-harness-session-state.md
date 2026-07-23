# SPEC-tau-harness-session-state: Session State

## Record justification

Session state spans durable session and agent stores, harness cold restoration,
runtime routing and navigation classification, UI roster projection, and
extension-owned state. No component-local documentation can describe their
shared lifecycle and recovery invariants coherently.

## Session and agent stores

The session store owns durable membership facts such as
`session.agent_loaded` and `session.agent_unloaded`. `session.started` and
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
policy/config files, and runtime sockets are also outside the session-ephemeral
boundary.

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
directories, sidecars, locks, or event files. Durable `events.cbor` replay
validates framing, monotonic durable sequence numbers, path-safe store IDs, and
the same semantic event/parent invariants as live append. Corrupt, truncated,
spliced, or semantically invalid records fail with a typed store error rather
than being skipped or partially folded.

Durable agent, ordinary-session, and session-restore appends capture the exact
journal EOF before writing a length-prefixed CBOR frame. Prefix, payload, or
commit-sync failure rolls the journal back to that EOF and durably syncs the
truncation before returning the original append error. A live store that cannot
complete either rollback operation rejects later appends to that journal without
touching it. Sequence, folded state, agent checkpoints, and session metadata
advance only after the frame commit succeeds; strict replay never salvages a
valid suffix after a partial frame.

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
tests.

Each debug JSON object and its trailing newline are serialized before the file is
touched, then appended under the existing `Write::flush` policy. Under the
session's single-writer lock, a returned write or flush error truncates the file
to its exact prior EOF. If truncation or its rollback flush fails, the process
disables later debug-log writes rather than appending after an uncertain line
boundary. This non-authoritative diagnostic mirror does not promise crash or
power-loss durability: termination can leave a missing or torn final line, and
restart neither repairs nor salvages that line.

`tau_harness::debug_log_timing` traces exact monotonic microseconds for the
whole write cycle and its serialization, EOF lookup, write/flush, and rollback
phases, plus line byte count, start/end EOF, event name, and result class. Cycles
over 500 milliseconds emit the same content-free fields at warning level. These
diagnostics remain tracing output only: they never write recursively to
`events.jsonl` and do not alter append, flush, rollback, or failure behavior.

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
[DECISION-interactive-user-prompt-envelope](../../../specs/DECISION-interactive-user-prompt-envelope.md).

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
redispatch. It publishes one harness-authored nonsemantic `tool.error` followed
by one durable agent-owned `provider.tool_error` with the restart and
possible-side-effect diagnostic. The durable provider error balances the
provider context and makes later cold resumes idempotent; an already repaired
call receives no second pair. Canonical terminal publication follows
[SPEC-terminal-tool-reports-and-canonical-outcomes](../../../specs/SPEC-terminal-tool-reports-and-canonical-outcomes.md).

New durable agents commit `AgentStarted` as journal sequence zero before the
harness publishes their route or session membership. Loading an existing agent
into another session publishes membership but never appends a second creation
fact. A sidecar-only artifact reserves its id but is not a semantic routing
identity. See
[DECISION-tau-core-agent-summary-checkpoints](../../tau-core/specs/DECISION-tau-core-agent-summary-checkpoints.md).

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
entrypoints retain their separate ordinary-agent lifecycle. See
[DECISION-cold-restored-completed-worker-ownership](../../../specs/DECISION-cold-restored-completed-worker-ownership.md).

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
