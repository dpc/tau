# DECISION-compact-prompt-materialization-authority: Compact prompt materialization authority

Authority: confirmed, 2026-07-24, dpc

## Decision

The exact historical full provider prompt is not semantic authority. Canonical
transcript facts and compaction replacement facts remain provider-content
authority. An inference dispatch checkpoint or standalone-compaction start is
the sole durable recovery and no-resend owner, and the corresponding terminal
outcome remains completion authority.

The harness-authored `agent.prompt_started`, including its operation, is the
durable fact that a full provider request was materialized and admitted for
dispatch. It establishes that prompt's unique durable materialization identity
and advances ordinary inference generation only for the inference operation.
It does not assert that a provider received the request. Only the harness may
author this fact, and it must match exactly one unresolved durable owner by
agent, prompt, model, and operation.

`agent.prompt_created` is a transient work envelope. It remains available
through its existing protected live interceptor path and existing transient,
projected observer path, and is routed point-to-point to the selected provider,
but it does not enter semantic storage. Generic observer projections remain
byte-free, while the selected provider receives canonical typed content.
Provider connection identity remains runtime state and is not part of the
compact durable fact.

### Commit and delivery boundary

The durable owner must semantically append before full prompt materialization.
The compact prompt-start fact must then complete its foreground frame write
before the full request can leave the harness for provider delivery. The full request must be
owned by that compact fact's post-commit continuation rather than published
independently or released by FIFO timing.

Each durable owner and `(agent_id, agent_prompt_id)` admits at most one compact
materialization fact and one live continuation. A duplicate live compact append
must be rejected before it can acquire a continuation, and delivery consumes
that continuation once. Replay never recreates it.

Immediately before provider send, delivery must require the same current
session and loaded runtime incarnation that
admitted the request, an unresolved owner, and its unique compact fact. The
full request, compact fact, and owner must agree on agent, prompt, model, and
operation. The current route is resolved from that model; any failed check
fails closed. Failure to complete the compact fact's foreground append makes
delivery of its full request impossible. Background sync failure does not block
delivery. Recovery never reconstructs or resends the
transient request, including after a crash between owner commit, compact-fact
commit, and provider delivery.

### Incompatible journal format

For each prompt that reaches compact materialization admission, a new durable
agent journal contains exactly one compact prompt-start fact. A journal prefix
ending after owner commit but before materialization may contain none and
remains valid under the no-resend recovery rule. A second compact fact for the
same prompt ID is invalid and must be rejected during live append and strict
replay. Only an inference operation advances ordinary inference generation,
while standalone compaction remains excluded.

Persisted full `agent.prompt_created` records are unsupported legacy journal
data. This format change provides no old/full decoding, mixed old/full and
new/compact deduplication or precedence, migration, backfill, or rewrite.
Users must discard or reset old agent journals rather than open them under the
new format.

Compact prompt-start facts participate in cold folding and audit but remain
excluded from historical subscriber catch-up; replay never re-emits them as
provider work or releases a full request. Any future subscriber replay requires
a separate approved extension-interface decision. These incompatibilities
follow
[DECISION-no-backward-compatibility](DECISION-no-backward-compatibility.md).

### Diagnostic and extension boundary

The best-effort `events.jsonl` projection of a full prompt must be a bounded,
content-free summary whose size does not grow with prompt history or image
bytes. It remains non-authoritative and may describe an attempt whose later
semantic append failed.

Exact provider-request capture is optional diagnostic output, not semantic
state. It must be explicitly enabled, bounded by retention policy, and never
consulted for folding, replay, recovery, resend, or generation. Concrete
retention limits and failure-sampling policy are outside this decision.

This decision preserves existing transient prompt-created visibility for
observers through their existing byte-free projection and preserves the
protected interceptor path. `GetAgentPromptCreated` remains best-effort: `None`
is valid after the live payload expires, with no durable lookup, retention, or
reconstruction obligation. Restricting live visibility to the selected
provider is a separate extension-interface decision.

### Exclusions

This decision does not create historical request reconstruction, prompt blobs
or content-addressed storage, copied-prompt quota charges, or compressed full
records as alternate authority. It does not make an exact provider wire request
durable authority, change canonical transcript or compaction image ownership,
or establish the internal materialized-prompt counter as a public contract.

## Rationale

Durable full prompts repeatedly snapshot growing transcript, tool, system, and
image content even though recovery does not use those snapshots. The compact
fact preserves the generation guard and a bounded audit join, while
append-coupled transient delivery preserves semantic ordering and no-resend
semantics without creating a second content authority. It deliberately does not
provide a durability barrier before provider effects.

The existing prompt-start fact already denotes a fully materialized request
admitted before provider receipt, so reusing it avoids a new protocol concept
with no distinct authority.

This decision refines the generic publication and persistence contract in
[DECISION-generic-peer-event-emission](DECISION-generic-peer-event-emission.md)
and is governed by
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md).
Its former append-and-sync delivery barrier is superseded by
[DECISION-semantic-journal-writeback-durability](DECISION-semantic-journal-writeback-durability.md).
