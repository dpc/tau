# DECISION-tau-cli-agent-watch-state-authority: Agent watch state authority

Authority: confirmed, 2026-07-07, dpc

The CLI keeps display identity, navigation eligibility, and execution state
separate. Complete structured watch and outer-turn snapshots are authoritative for
watched activity; agent names, being watched, being non-suspended, prompt/provider
heuristics, and stats alone do not establish that an agent is running.

This avoids encoding topology in names and prevents inner model rounds or
navigation policy from creating false running indicators. Exact projection,
ordering, fallback, cache, and reset behavior is specified by
[SPEC-tau-cli-agent-message-labels](SPEC-tau-cli-agent-message-labels.md).
