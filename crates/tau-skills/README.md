# Tau built-in skills

This crate packages Tau's built-in skills. Extension-owned skills may retain
relative links from their standalone source; the sections below keep those
links useful in Tau's embedded copy.

## Migration from removed keys

For Slack, replace `channel_ids`, `listening_scope`, and `send_destinations`
with explicit `conversations` records. Give each old conversation an alias and
kind, map the old listening scope to `receive`, and set `proactive_send: true`
where the destination was available for proactive sends. Replace the old
empty-channel implicit-DM mode with `dynamic_direct_messages`.

The removed `prefix_agent_id` option has no replacement. Replies and proactive
sends now use the supplied message unchanged. Use separate extension instances
with distinct `tool_prefix` values when roles must not share route inventory.

The separately maintained
[`tau-ext-slack` project](https://radicle.network/nodes/radicle.dpc.pw/rad%3Az3NJhEtKWCbHPa28wDQSYJ8eEfBjg)
owns the complete migration documentation.

## Troubleshooting

For a Slack Socket Mode reconnection loop, restart the harness with:

```sh
TAU_LOG='slack=trace,warn' tau
```

The running extension reads this filter only at process startup. Find the
session with `tau session list`, then inspect the configured extension
instance's log under
`${XDG_STATE_HOME:-$HOME/.local/state}/tau/sessions/<session_id>/logs/`.
Review private logs before sharing them.

The separately maintained
[`tau-ext-slack` project](https://radicle.network/nodes/radicle.dpc.pw/rad%3Az3NJhEtKWCbHPa28wDQSYJ8eEfBjg)
owns the full diagnostic classification and reconnection runbook.
