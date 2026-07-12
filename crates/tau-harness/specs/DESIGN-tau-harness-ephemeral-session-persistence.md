# DESIGN-tau-harness-ephemeral-session-persistence: Ephemeral sessions suppress only session-owned persistence

Status: unconfirmed

`tau --ephemeral` is a harness/session launch mode, not an agent privacy mode.
It keeps the live session state machine, interception, prompt dispatch, and
agent stores working normally, but session-owned persistence is runtime-only for
that harness process: session membership logs, session metadata/locks,
`events.jsonl`, per-session stderr logs, and session-scoped extension data are
not written. `harness.session_dir` uses status `ephemeral` and a display-only
`<ephemeral>` path so UIs do not advertise a usable session directory.

Agent transcripts remain durable by default, including sub-agents started by
`agent_start`. Per-agent ephemerality is a separate creation policy staged from
the TUI with `/new` then `/ephemeral on`; it keeps that agent's semantic
transcript, metadata, and session membership in memory until daemon exit.
Children of ephemeral parents inherit ephemerality. Provider state, credentials,
policy/config files, runtime sockets, user/cache extension data, durable
recipients/parents, and tool side effects keep their normal persistence.
