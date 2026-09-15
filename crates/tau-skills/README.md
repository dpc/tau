# Tau built-in skills

This crate packages Tau's built-in skills. Extension-owned skills may retain
relative links from their standalone source; the sections below keep those
links useful in Tau's embedded copy.

## Role-filtered context

Skills and AGENTS files support the same optional YAML frontmatter lists:

```yaml
---
only-roles: [researcher-senior]
only-role-groups: [engineer]
except-roles: [engineer-junior]
except-role-groups: [reviewer]
---
```

The two `only` lists are alternatives: either role or group may allow the file.
Then any `except` match excludes it. Missing both `only` lists allows everyone;
an explicitly empty list allows nobody unless the other dimension matches.
Names are exact and case-sensitive; unknown names match nothing. Group names
are actual configured groups, not role-name prefixes.

Filtering selects context, not filesystem permissions. Collision winners are
chosen first; hiding a winner never reveals a duplicate. Eligibility freezes
when an agent initializes, while later skill loads still read current bodies.
AGENTS files stack independently and their headers are removed from context.

Malformed filters ignore all four lists, preserve useful content and valid
unrelated metadata, and produce a file-specific UI warning. Unparseable headers
preserve raw instructions rather than discarding them. Warnings remain available
to UIs that attach later.
Skills without a recoverable valid name in either header or path are discarded
with a corrective warning; Tau does not invent aliases or bypass bounded reads.
Minor auxiliary metadata errors do not justify hiding recoverable instructions.

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
