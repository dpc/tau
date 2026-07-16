# SPEC-tau-cli-agent-message-labels: Agent message endpoint labels

The CLI presents an agent endpoint in harness-owned message activity as its
unambiguous routing id followed by a supplemental display name in parentheses
when authoritative metadata for that endpoint is known. Sender and recipient
names are resolved independently. User endpoints remain `user`, and unknown
local or peer endpoints remain id-only.

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
historical blocks; hidden transcript snapshots re-render when selected.
Each block retains its originating session as presentation provenance, so a
same-spelled agent in a subsequently resumed session can never relabel older
history. Replay therefore produces the same presentation after that session's
metadata has folded without rewriting the immutable message event or its body.

Watch-response and watch-prompt projections use the same endpoint formatter,
while their source/recipient wording and structured lifecycle rendering remain
unchanged. Canonical transport endpoints retain their explicit transport and
session qualification.

This specification refines
[DESIGN-tau-cli-agent-watch-display](DESIGN-tau-cli-agent-watch-display.md) and
preserves the lifecycle distinctions in
[DESIGN-tau-cli-watch-lifecycle-rendering](DESIGN-tau-cli-watch-lifecycle-rendering.md).
