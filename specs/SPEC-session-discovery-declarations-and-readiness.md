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
Registered session-context providers must either publish their mandatory ordered
discovery and readiness transaction or fail the connection so disconnect
handling removes their wait source; successful connection handling cannot
silently omit readiness.

The harness waits for exact readiness or disconnect from every outstanding
registered provider, bounded by a non-renewable thirty-second absolute
deadline. Provider silence, generic events, snapshots, stale or wrong-session
declarations, and traffic from providers no longer outstanding do not complete
or extend the wait. Final waiter removal takes precedence over deadline
classification, and synchronous harness-owned finalization after that readiness
cannot retroactively fail with a provider timeout. Absolute expiry reports
`SessionInitTimeout`, distinct from extension process `StartupTimeout`.

Each live agent load carries a fresh mandatory `agent_initialization_id`. Its
pending discovery state is seeded from the completed session baseline. Providers
selected by `session.agent_loaded` may atomically replace their source in that
pending state with `extension.agent_discovery_snapshot_declared`, publish
correlated keyed context, and acknowledge the same initialization. One agent's
snapshot or readiness cannot settle another agent.

Ready-before-snapshot finalizes the seeded baseline. Duplicate snapshots replace
the pending source; duplicate readiness is inert. Wrong session, agent,
initialization id, connection generation, unsolicited post-finalization declarations, and
unload-time late traffic are effect-free. Disconnect removes the source from
pending state and its wait set, but never mutates a frozen agent snapshot.
Bound workdir refreshes have the source-local correlation and degraded fallback
specified by [SPEC-per-agent-context-declarations-and-readiness](SPEC-per-agent-context-declarations-and-readiness.md).

## Finalized state and consumers

After the final waiter settles, the harness checks effective skill sources for
loadability and falls back through collision candidates when possible. It then
filters winners for the agent's effective role and configured group, independently
filters ordered AGENTS.md files, validates required skills against the eligible
set, and renders included instructions without frontmatter. It publishes one durable
`agent.initialization_context_set` replacement fact for that exact initialization,
including an unchanged cold-restored initialization with a fresh ID. The reducer
stores the latest durable fact as agent side state without creating a transcript
node or advancing the branch
head. Missing AGENTS.md files clear the bootstrap slot on the next initialization
or applicable project refresh.

The committed fact freezes the agent's effective skills and bootstrap block for
the load attempt. Provider `<available_skills>`, the model `skill` tool,
selected-agent `:skill` expansion, and the protected transient
`harness.agent_context_initialized` projection all consume that frozen state.
Non-literal `:skill` commands wait for pending discovery before expansion.
Unsolicited session/source updates do not mutate an already-frozen agent;
canonical workdir mutations explicitly replace only their bound source.

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

The agent current-state projection includes the complete frozen eligible
`effective_skills` set separately from advertised `listed_skills`. Selected-agent
completion consumes only that eligible set, including unadvertised user-invocable
skills. An older projection omitting the eligible set yields empty completion,
never a fallback to the role-neutral session inventory. Prospective-agent
completion filters the session baseline by the effective role and configured group.

Configured extensions are trusted local executables subject to the authority and
resource boundaries in [`SECURITY.md`](../SECURITY.md).

## Role-filtered context

Skills and every independently stacked AGENTS file share four optional YAML
list fields: `only-roles`, `only-role-groups`, `except-roles`, and
`except-role-groups`. Only dimensions form a union first, then any except match
excludes. Missing both only fields admits all; an explicit empty only list
admits none unless the other only dimension matches. Names are exact,
case-sensitive, nonempty, and unpadded. Unknown names are inert. Group identity
comes from configured membership, with the existing role-name fallback.

Policy cannot reveal a lower collision candidate when the selected loadable
winner is hidden. Session inventory stays role-neutral; required-skill preflight
evaluates each prospective role independently. All initialized consumers,
including advertisement, exact/search/content model access, `:skill`, selected
completion, and role previews, use that agent's eligible frozen set. File bodies
remain live reads, but editing a header does not resample eligibility until a
new initialization or applicable project refresh. Unchanged user and other-source
eligibility is not resampled by a project refresh. This does not restrict direct
filesystem access.

An unavailable per-agent required skill rejects a fresh initialization before the
durable replacement is published. Accepted delegated starts use their existing
correlated failure terminal; pending previews and initial UI prompts receive
their existing failure responses and the rejected runtime is unloaded. Other
agents and the harness remain operational. Session-level selected/default-role
preflight failure remains a fresh-session startup error. Resume preserves role
definitions and defers validation to each agent's restored project context.
Previously initialized agents remain loaded with explicit missing-required-skill
diagnostics so they can repair an unavailable cwd; repair clears those diagnostics.

Malformed headers and filters follow
[REQ-context-file-frontmatter-fail-open](REQ-context-file-frontmatter-fail-open.md).
Recognized invalid policy discards all four filters while preserving valid
unrelated metadata. Unparseable headers preserve raw useful instructions.
File-specific diagnostics travel with the admitted source snapshot, including
agent-only discovery; stale or dropped snapshots cannot alert. The harness
reserves existing snapshot capacity for mandatory diagnostics before ordinary
inventory items can consume it. The harness
retains Warning+Alert notices for late subscribers and diagnostic filtering
cannot suppress them. Encountered malformed live bodies warn through the same
alert path without changing sampled eligibility.
