# DECISION-agent-initialization-discovery-snapshots: Atomic agent initialization discovery

Authority: confirmed, 2026-07-26, dpc

## Decision

Tau will replace positive, item-at-a-time skill and AGENTS.md discovery with
complete, atomic snapshots from each configured extension connection. The wire
events are `extension.session_discovery_snapshot_declared` for the session
baseline and `extension.agent_discovery_snapshot_declared` for one agent
initialization. Each snapshot contains both skill candidates and ordered
AGENTS.md files. Omission removes an old contribution and an empty snapshot
clears the source.

The raw declarations remain extension-authored, interceptable, and
`persist=false`. Ordinary publication must commit before validation or other
effects. The harness revalidates the exact configured connection generation and
session after interception and before applying a declaration. It stages only the
latest complete declaration for each source and scope, with explicit item and
decoded-byte bounds in addition to existing activation and frame bounds.

The harness maintains a current session baseline and creates a separate pending
snapshot, initially seeded from that baseline, for every agent initialization.
Every load receives a fresh `agent_initialization_id`; agent-loaded,
agent-discovery, per-agent context, and readiness messages carry it. A declaration
or acknowledgement can affect only its exact session, agent, initialization, and
connection generation. The harness freezes the effective snapshot after all
captured context providers settle. Disconnect removes that source from the
session baseline and pending initializations, but never changes a finalized
agent snapshot. Initializing one agent therefore cannot change another agent's
prompt or skill surface.

Within one accepted source snapshot, the first duplicate skill name or canonical
AGENTS.md path wins and later duplicates produce diagnostics. Invalid,
unreadable, or oversized entries are diagnosed and omitted from the new source
set before its atomic swap; one invalid entry does not retain the source's stale
old set. Surviving source slots keep their ordinals. Skill collisions retain the
current greatest-sampled-mtime rule, with first insertion winning equal or
unavailable mtimes, and removal recomputes fallback winners. AGENTS.md files
retain scanner order from broad to specific and stable source/slot order.

The initialization check verifies bounded skill metadata and loadability, but
does not snapshot complete skill contents. A later filesystem change may still
make the `skill` tool read fail. This preserves Tau's current trusted mutable-path
model rather than expanding this work into content-addressed skill storage.

At every finalization the harness commits one durable
`agent.initialization_context_set` replacement fact before releasing the first
prompt. The fact carries the exact session, agent, and
`agent_initialization_id` together with the bootstrap content and effective
snapshot. Cold-restored initialization therefore records its fresh exact
initialization ID even when effective content is unchanged. The reducer folds
the latest durable value into a dedicated per-agent bootstrap slot and frozen
skill snapshot without advancing the conversation branch. Prompt
assembly prepends the rendered AGENTS.md stack as a user-role instruction block
outside ordinary compactable transcript history. Empty content clears the slot;
AGENTS.md is never appended as an ordinary user transcript node by initialization.
Cold replay reconstructs the latest slot, and a fresh initialization replaces it
before prompt dispatch.

The harness then publishes protected, transient current-state projections:

- `harness.agent_context_initialized` carries the exact session, agent, and
  `agent_initialization_id`, the exact skills rendered in that agent's
  `<available_skills>`, and the exact ordered AGENTS.md path, line, and byte
  summaries used for its bootstrap block;
- `harness.session_skills_available` contains the complete validated,
  collision-resolved session skill state needed for role preflight and manual
  `:skill` completion.

Only the harness may author these projections. Live subscribers receive each
replacement, while late subscribers receive one synthesized current session
snapshot and one current projection per loaded agent after its transcript replay
boundary. Raw declarations and historical initialization projections are neither
replayed nor synthesized.

**Required-skill refresh failures retain the session-baseline role contract.**
Role availability and required-skill preflight remain determined by the validated
session baseline. A later agent-specific refresh that omits or cannot load a
required skill does not retire a new agent, block a restored agent, or add a
second role-validity gate. The finalized agent snapshot remains truthful even
when it is narrower than the baseline.

**The initialization display contains only model-listed skills and bootstrap
AGENTS.md files.** It renders exactly the skills in that agent's
`<available_skills>` plus the AGENTS.md summaries used for bootstrap. Separately
user-invocable/manual skills remain available through canonical session skill
state for `:skill` completion, but the UI must not present them as skills listed
to the model.

The old `extension.skill_available` and `extension.agents_md_available` events
are removed rather than supported in parallel. No persisted-data or protocol
migration is provided. Histories containing old ordinary AGENTS.md injections
may retain those transcript nodes; operators must reset or discard pre-change
internal state when clean replacement semantics matter.

## Rationale

Positive item announcements cannot express deletion, expose intermediate
collision winners, and repeatedly append growing instruction stacks. A complete
source replacement makes refresh atomic and permits empty/deleted state. Freezing
one initialization snapshot per agent keeps every model and UI surface truthful
without allowing a later agent load to mutate an earlier agent.

A durable replacement slot preserves event-log-first reconstruction while making
the newest instructions authoritative instead of accumulating stale transcript
history. Protected current-state projections let live and late UIs report
accepted harness state rather than raw extension candidates. Mandatory
initialization correlation prevents delayed frames from an earlier load of the
same durable agent from settling or mutating a later load.

The session-baseline required-skill rule is the narrowest extension of the
existing session-global role contract; per-agent retirement and restore blocking
would add lifecycle policy not required by this feature. Restricting the display
to `<available_skills>` and bootstrap files implements the ticket's literal
“listed to the agent” scope without conflating manual command completion with
model-visible context.

This decision supersedes the item-declaration and append-only injection choices in
[DECISION-session-discovery-declarations-and-readiness](DECISION-session-discovery-declarations-and-readiness.md)
while preserving its configured-connection authority and commit-before-effects
boundary. It also supersedes the discovery row of
[DECISION-generic-peer-event-emission](DECISION-generic-peer-event-emission.md)
only where that row retains the old event names and lacks the snapshot-to-canonical
projection split; its generic publication, authorship, interception, and
commit-before-effects rules remain governing. The decision follows
[DECISION-event-log-first-extension-state](DECISION-event-log-first-extension-state.md),
[DECISION-positive-persistence-publication-metadata](DECISION-positive-persistence-publication-metadata.md),
[DECISION-instruction-symlink-discovery](DECISION-instruction-symlink-discovery.md),
and [DECISION-no-backward-compatibility](DECISION-no-backward-compatibility.md).
It was separately reviewed and confirmed under
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md)
before implementation. Current-state architecture and specifications change
with the implementing change.
