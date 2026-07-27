# SPEC-session-discovery-declarations-and-readiness: Atomic discovery snapshots and readiness

## Record justification

Discovery spans protocol admission, interception, extension activation, shell
scanning, harness collision resolution, role preflight, agent initialization,
prompt/tool consumers, and UI current-state projection; no component owns the
complete contract.

This specification implements the discovery row of
[SPEC-peer-event-publication](SPEC-peer-event-publication.md).

## Publication and atomic replacement

Configured local extensions may register with
`extension.session_context_provider_register`, publish complete
`extension.session_discovery_snapshot_declared` source snapshots, and acknowledge
with `extension.session_context_ready`. Raw declarations are transient,
interceptable observations. Admission captures the exact configured connection
generation; stale, unconfigured, socket, wrong-session, dropped, malformed, or
over-limit declarations cannot mutate discovery state.

A committed valid snapshot atomically replaces that connection's complete skill
and ordered AGENTS.md contribution. An empty list clears that contribution.
Validation omits an invalid individual item without exposing partial replacement;
duplicate names or canonical paths retain the first item. Snapshot item count,
decoded bytes, individual AGENTS.md content, protocol frames, and activation
staging are bounded.

Skill winners retain stable source slots. The candidate with the greatest sampled
mtime wins; first insertion wins when mtimes compare equal or are unavailable.
Same-source updates replace surviving slots in place, deletion removes slots,
rename appends the new name, and source disconnect recomputes fallback winners.
The harness publishes a complete protected
`harness.session_skills_available` projection after each accepted replacement and
at session readiness. Role required-skill preflight and agentless CLI completion
consume this session baseline.

## Readiness and agent initialization

Session wait sets contain registered live non-socket Tool connections whose live
selectors match `session.started`. Only matching
`extension.session_context_ready` releases that exact wait. Registration and
snapshot declarations settle before readiness through the ordinary FIFO
interception boundary.

Each live agent load carries a fresh mandatory `agent_initialization_id`. Its
pending discovery state is seeded from the completed session baseline. Providers
selected by `session.agent_loaded` may atomically replace their source in that
pending state with `extension.agent_discovery_snapshot_declared`, publish
correlated keyed context, and acknowledge the same initialization. One agent's
snapshot or readiness cannot settle another agent.

Ready-before-snapshot finalizes the seeded baseline. Duplicate snapshots replace
the pending source; duplicate readiness is inert. Wrong session, agent,
initialization id, connection generation, post-finalization declarations, and
unload-time late traffic are effect-free. Disconnect removes the source from
pending state and its wait set, but never mutates a frozen agent snapshot.

## Finalized state and consumers

After the final waiter settles, the harness checks effective skill sources for
loadability, falls back through collision candidates when possible, renders all
ordered AGENTS.md files once, and publishes one durable
`agent.initialization_context_set` replacement fact for that exact initialization,
including an unchanged cold-restored initialization with a fresh ID. The reducer
stores the latest durable fact as agent side state without creating a transcript
node or advancing the branch
head. Missing AGENTS.md files clear the bootstrap slot on the next initialization.

The committed fact freezes the agent's effective skills and bootstrap block for
the load attempt. Provider `<available_skills>`, the model `skill` tool,
selected-agent `:skill` expansion, and the protected transient
`harness.agent_context_initialized` projection all consume that frozen state.
Initial non-literal `:skill` commands wait for finalization before expansion.
Later session/source updates do not mutate an already-frozen agent.

The bootstrap block is materialized once as a provider user-context block outside
ordinary transcript history. Branching and compaction retain the latest folded
slot exactly once; attaching a UI cannot append or duplicate it.

## Replay and current state

Cold resume refreshes session discovery and starts a new correlated initialization
for every restored live agent before replay activations or prompts dispatch. Each
refresh replaces the durable initialization side state rather than appending an
ordinary AGENTS.md user message.

Raw declarations and readiness have no historical replay. Late subscribers receive
one current `harness.session_skills_available` snapshot and one
`harness.agent_context_initialized` projection per live initialized agent, with no
raw declarations or prompt side effects.

Configured extensions are trusted local executables subject to the authority and
resource boundaries in [`SECURITY.md`](../SECURITY.md).
