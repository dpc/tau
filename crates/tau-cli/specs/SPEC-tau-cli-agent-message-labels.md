# SPEC-tau-cli-agent-message-labels: Agent message endpoint labels

## Record justification

Because agent identity and activity presentation spans transcript event folding,
session-scoped metadata, watch and navigation caches, message and status-row
rendering, and lifecycle replay, no single owning module can state their
combined identity, authority, and projection rules coherently.

The CLI presents an agent endpoint in harness-owned message activity as its
unambiguous `@`-prefixed routing id followed by a supplemental display name in
parentheses when authoritative metadata for that endpoint is known. Sender and
recipient identities use the bright `agent.message.identity` theme style while
the surrounding wording and supplemental task-name context remain intact.
Sender and recipient names are resolved independently. Unknown local or peer
endpoints remain id-only.

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

Ordinary directed communication, including watched responses, is labeled
`Message`. In a selected-agent transcript the selected endpoint is implicit:
received messages show only `■ Message from <sender>`, and sent messages show only
`■ Message to <recipient>`. The no-selection overview shows both endpoints as
`■ Message from <sender> to <recipient>`. Message bodies remain below these
headers. Watch-prompt projections keep their distinct lifecycle wording while
using the same endpoint formatter. Canonical transport endpoints retain their
explicit transport and session qualification.

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

Current CLI watched status rows come from the current-session semantic
`WorkStatus` snapshot for each selected target. Absent, unreported, and unknown
status renders as `❓`; working, blocked, and done render as `🚀`, `⛔️`, and `✅`;
unreported, working, blocked, and unknown remain visible, and done alone
removes the row. The target's complete generic agent-stats detailed turn activity controls
activity decoration. Before the first stats snapshot, active
prompt tracking for the target is the compatibility/catch-up fallback.

The CLI independently selects rows and derives recursive activity over the
current watch graph. Row selection starts from the agent whose UI is viewed,
traverses forward edges cycle-safely, excludes the root, and deduplicates each
target onto a shortest path with lexicographic path ties. It traverses through
Done targets while hiding their rows and retains topology targets that lack
stats or status. Up to eight visible targets produce the complete closure; a
ninth switches atomically to all visible direct watches, without truncating the
direct set. Expanded rows use `(depth, agent-id)` order. Each indirect row shows
its deterministic immediate predecessor as `via @parent`; the `via` label retains
agent-context styling while `@parent` uses the same watched-agent identity style
as the row's primary `@id`.

A direct target renders as `<turn-emoji> <phase-emoji> @id (display name)
title`, followed by existing tool/context telemetry. The two fixed-width emoji and
stable id form the mandatory leftmost prefix;
the display name is optional persisted UI metadata; phase/title are the watched
agent's own structured `WorkStatus` report. Under width pressure the display
name yields before the title, while identity and phase retain their existing
higher priority. An otherwise non-Running edge whose target watches an effective
descendant adds `watching -> @witness`, where the witness is the nearest directly
running descendant and equal-depth candidates use stable agent-id order. Direct
activity wins when both apply. Indirect `via @parent` context and
`watching -> @witness` activity are independent and may coexist.

The session-wide `@N` count deduplicates all recursively effective watch targets
and excludes the selected agent. Active prompts outside every watch edge retain
their compatibility fallback contribution.

## Lifecycle projection

Harness-authored watched-agent `WorkStatus` records are structured
state rather than ordinary messages. Working, done, and blocked reports render
as `▤ Status update from <sender>: <phase-emoji> (<reported task>)`, suppress their
empty compatibility body, and bypass `show-messages`.

Harness-authored `WatchProviderStatus` records render as `□ [tau-internal]:`
notices, or `□ [tau-internal current snapshot]:` when `initial` is true. The
renderer removes only the exact canonical outer
`<tau_internal>...&lt;/tau_internal&gt;` frame; nonmatching, partial, nested, and
legacy presentation text remains verbatim after the label. `WatchLongWait`
records remain `▤` status rows and derive a nonempty summary from their typed
threshold because their producer body is empty. Both bypass `show-messages` and
never become ordinary message blocks. See
[SPEC-tau-cli-transcript-context](SPEC-tau-cli-transcript-context.md) for the
matching live and replay transcript projection.

Genuine watched responses and direct-user-prompt notifications retain their
`WatchResponse` and `WatchPrompt` kinds, sender/watcher attribution, history
classification, and summary/full/hidden visibility behavior.

This specification is constrained by
[SPEC-agent-watch](../../../specs/SPEC-agent-watch.md).
