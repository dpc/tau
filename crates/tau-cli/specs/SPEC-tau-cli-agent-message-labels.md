# SPEC-tau-cli-agent-message-labels: Agent message endpoint labels

## Record justification

Agent identity and activity presentation spans transcript event folding,
session-scoped metadata, watch, and navigation caches, message and status-row
rendering, and lifecycle replay. No single owning module can state their combined
identity, authority, and projection rules coherently.

The CLI presents an agent endpoint in harness-owned message activity as its
unambiguous `@`-prefixed routing id followed by a supplemental display name in
parentheses when authoritative metadata for that endpoint is known. Sender and
recipient identities use the bright `agent.message.identity` theme style while
the surrounding wording and supplemental task-name context remain intact.
Sender and recipient names are resolved independently. User endpoints remain
`user`, and unknown local or peer endpoints remain id-only.

Local names come from the session's folded `agent.started` and
`agent.display_name_set` metadata, including replayed metadata for restored or
currently unloaded agents. A cross-session endpoint must not borrow the name of
a same-spelled local agent. It may show a remote name only when the typed
endpoint itself carries presentation metadata advertised by that peer.

Names are presentation-only. They never alter message bodies, routing
identities, semantic transcript events, trust decisions, or provider context.
The CLI visibly escapes controls, preserves whole Unicode graphemes, and
truncates supplemental names to bounded byte and terminal-column limits. It
omits a name that contains the agent id instead of displaying redundant
identity text.

Message blocks are current-state projections rather than event-time name
snapshots. A later authoritative display-name update re-renders visible
historical blocks; hidden transcript snapshots, including the all-agent
no-selection overview, re-render when selected.
Each block retains its originating session as presentation provenance, so a
same-spelled agent in a subsequently resumed session can never relabel older
history. Replay therefore produces the same presentation after that session's
metadata has folded without rewriting the immutable message event or its body.

Watch-response and watch-prompt projections use the same endpoint formatter,
while their source/recipient wording and structured lifecycle rendering remain
unchanged. Canonical transport endpoints retain their explicit transport and
session qualification.

## Watch state and navigation

The CLI derives session-scoped forward and reverse watcher caches from complete
`agent.watches_updated` snapshots. It clears them on session reset and rebuilds
them from live or replayed snapshots; they do not mutate display names or durable
transcripts.

For the currently viewed agent, one watcher renders `<watcher-id>`.
Multiple watchers are sorted by stable agent id and render the first watcher as
`<first-id>, +N more agents`; remaining ids are not expanded into the
status row. Watcher context never changes the agent's own display label.

Navigation state (`active`, `active-auto`, or `suspended`) is harness-owned
current-session daemon memory projected into each UI by agent stats. `active` is
always eligible, `active-auto` is eligible only while stats report a running
runtime, and `suspended` is ineligible. Delegated agents default to
`active-auto`; ordinary agents default to `active`. Selection and presentation
remain UI-local. Keyboard previous/next navigation forms a ring containing the
no-selection overview followed by the active agents in stable known-agent order;
suspended agents are skipped. Modes do not affect loading, addressability, or
delivery.
Selection alone preserves mode. Successful admission of direct visible human
input to the selected existing agent makes that exact target `active` for
subsequent navigation; complete harness stats, not the local prompt event, update
the CLI cache.

Current CLI activity comes from the latest watched-agent `TurnState` record
cached on each directed watch edge. `Running` renders as active and `Idle` does
not. Before an edge receives its first `TurnState`, active prompt tracking for
the target is the edge-local compatibility/catch-up fallback. The CLI does not
yet consume structured `WorkStatus`; semantic status migration belongs to a
later explicitly approved client change.

The CLI derives recursive activity exactly over the current live watch DAG. A
direct target whose edge reports Running renders as `running [name] @id`. An otherwise
non-Running edge whose target watches an effective descendant renders as
`watching [name] @id -> @witness`, where the witness is the nearest directly
running descendant and equal-depth candidates use stable agent-id order. Direct
activity wins when both apply. Rows remain ordered by stable target id, and the
selected agent gets one row per direct target; recursive descendants are not
flattened into additional rows.

The session-wide `@N` count deduplicates all recursively effective watch targets
and excludes the selected agent. Active prompts outside every watch edge retain
their compatibility fallback contribution.

## Lifecycle projection

Harness-authored watched-turn lifecycle records are structured state, not
watched-agent messages. The CLI renders their structured payload as a compact
single-line status, suppresses any compatibility body, and bypasses
`show-messages`.

Genuine watched responses and direct-user-prompt notifications retain their
`WatchResponse` and `WatchPrompt` kinds, sender/watcher attribution, history
classification, and summary/full/hidden visibility behavior.

This specification is constrained by
[SPEC-agent-watch](../../../specs/SPEC-agent-watch.md).
