# DECISION-tau-cli-agent-watch-state-authority: Agent watch state authority

Authority: confirmed, 2026-07-18, dpc

The CLI keeps display identity, navigation eligibility, and execution state
separate. Complete structured watch and outer-turn snapshots are authoritative for
direct watched activity. Prompt/provider activity is a compatibility fallback only
until an edge receives its first structured snapshot. Agent names, being watched,
being non-suspended, and stats alone do not establish that an agent is running.

This avoids encoding topology in names and prevents inner model rounds or
navigation policy from creating false running indicators.

The CLI derives recursive watch activity exactly over the current session's live
watch DAG. A direct edge is effective when its edge-scoped lifecycle fact is
running; an otherwise idle target is effective when it watches an effective
descendant. Direct rows say `running`; transitive-only rows say `watching` and
identify the nearest directly running descendant, with stable agent-id tie
breaking. Direct state wins when both apply. The selected agent retains one row
per direct target, while the global `@N` count deduplicates all recursively
effective watch targets and preserves its existing selected-agent exclusion and
unwatched prompt fallback.

This projection changes no model notifications, harness lifecycle, persistence,
navigation eligibility, or routing. It uses only stable ids and live topology;
display names and historical agents do not participate.

Exact projection and presentation behavior is specified by
[SPEC-tau-cli-agent-message-labels](SPEC-tau-cli-agent-message-labels.md).
